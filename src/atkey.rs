//! Academic Torrents API-key handling: resolve the operator's key to a
//! per-user announce URL, and rewrite tracker lists to use it only on AT's
//! own https hosts.
//!
//! Security contract (preserved): the key and derived URLs carry the
//! account's passkey - they are sent only to academictorrents.com tracker
//! hosts, never logged, never written to disk.

use anyhow::{Context, Result};

pub const USER_ANNOUNCE_ENDPOINT: &str = "https://academictorrents.com/apiv2/userannounce";

/// Resolve the API key (`uid=N;pass=H`, valid Cookie syntax) to the
/// per-user announce URL plus its ipv6-host variant.
pub async fn resolve_user_announce(
    client: &reqwest::Client,
    api_key: &str,
) -> Result<(String, String)> {
    let resp = client
        .get(USER_ANNOUNCE_ENDPOINT)
        .header("Cookie", api_key)
        .send()
        .await
        .context("fetching user announce URL")?;
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .context("reading userannounce response")?;
    if !status.is_success() {
        anyhow::bail!("userannounce endpoint returned {status}");
    }
    let body = &body[..body.len().min(1 << 20)];
    let out: UserAnnounce =
        serde_json::from_slice(body).context("parsing userannounce response")?;
    if out.userannounce.is_empty() {
        anyhow::bail!("userannounce response had no announce URL");
    }
    let ipv6 = upgrade_to_ipv6_host(&out.userannounce);
    Ok((out.userannounce, ipv6))
}

#[derive(serde::Deserialize)]
struct UserAnnounce {
    #[serde(default)]
    userannounce: String,
}

fn upgrade_to_ipv6_host(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) if u.host_str() == Some("academictorrents.com") => {
            if u.set_host(Some("ipv6.academictorrents.com")).is_ok() {
                return u.to_string();
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// The per-user announce URL to use for a tracker URL, or None when the URL
/// is not an https URL on an AT tracker host (must never receive the key).
pub fn at_announce_url(
    announce: &str,
    user_announce: &str,
    user_announce_ipv6: &str,
) -> Option<String> {
    let u = url::Url::parse(announce).ok()?;
    if u.scheme() != "https" {
        return None;
    }
    match u.host_str()? {
        "academictorrents.com" if !user_announce.is_empty() => Some(user_announce.to_string()),
        "ipv6.academictorrents.com" if !user_announce_ipv6.is_empty() => {
            Some(user_announce_ipv6.to_string())
        }
        _ => None,
    }
}

/// Rewrite tracker tiers so AT trackers use the per-user URL. Third-party
/// trackers pass through untouched. Empty key => unchanged.
pub fn keyed_trackers(
    tiers: Vec<Vec<String>>,
    user_announce: &str,
    user_announce_ipv6: &str,
) -> Vec<Vec<String>> {
    if user_announce.is_empty() {
        return tiers;
    }
    tiers
        .into_iter()
        .map(|tier| {
            tier.into_iter()
                .map(|t| at_announce_url(&t, user_announce, user_announce_ipv6).unwrap_or(t))
                .collect()
        })
        .collect()
}

/// Filter tiers down to AT's own trackers (keyed when configured), dropping
/// third-party trackers. When no AT tracker is present at all, the list is
/// returned unchanged - zero trackers would leave no tracker discovery.
pub fn at_trackers_only(
    tiers: Vec<Vec<String>>,
    user_announce: &str,
    user_announce_ipv6: &str,
) -> Vec<Vec<String>> {
    let found = tiers.iter().flatten().any(|t| {
        at_announce_url(t, user_announce, user_announce).is_some() || is_at_tracker_url(t)
    });
    if !found {
        return tiers;
    }
    tiers
        .into_iter()
        .filter_map(|tier| {
            let kept: Vec<String> = tier
                .into_iter()
                .filter_map(|t| {
                    if is_at_tracker_url(&t) {
                        Some(at_announce_url(&t, user_announce, user_announce_ipv6).unwrap_or(t))
                    } else {
                        None
                    }
                })
                .collect();
            if kept.is_empty() {
                None
            } else {
                Some(kept)
            }
        })
        .collect()
}

pub fn is_at_tracker_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(u) => u
            .host_str()
            .map(|h| h.contains("academictorrents.com"))
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keying() {
        let tiers = vec![vec![
            "https://academictorrents.com/announce.php".to_string(),
            "udp://third:80/announce".to_string(),
        ]];
        let keyed = keyed_trackers(
            tiers.clone(),
            "https://academictorrents.com/announce.php?passkey=K",
            "",
        );
        assert_eq!(
            keyed[0][0],
            "https://academictorrents.com/announce.php?passkey=K"
        );
        assert_eq!(keyed[0][1], "udp://third:80/announce");

        let only = at_trackers_only(
            tiers,
            "https://academictorrents.com/announce.php?passkey=K",
            "",
        );
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].len(), 1);

        // No AT tracker: unchanged.
        let third = vec![vec!["udp://third:80/announce".to_string()]];
        assert_eq!(at_trackers_only(third.clone(), "K", ""), third);

        // http (non-https) AT URL never receives the key.
        assert_eq!(
            at_announce_url("http://academictorrents.com/announce.php", "K", ""),
            None
        );
    }
}
