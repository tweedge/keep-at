//! Self-update: check GitHub releases, download the matching asset,
//! atomically replace the running binary.
//! Linux-only asset naming: keep-at_linux_<arch>.tar.gz.

use anyhow::{Context, Result};

pub static RELEASES_URL: &str = "https://api.github.com/repos/tweedge/keep-at/releases/latest";
pub static RELEASES_LIST_URL: &str = "https://api.github.com/repos/tweedge/keep-at/releases";

#[derive(Debug, serde::Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn releases_url() -> String {
    std::env::var("KEEPAT_RELEASES_URL").unwrap_or_else(|_| RELEASES_URL.to_string())
}

fn releases_list_url() -> String {
    std::env::var("KEEPAT_RELEASES_LIST_URL").unwrap_or_else(|_| RELEASES_LIST_URL.to_string())
}

async fn fetch_release(client: &reqwest::Client, user_agent: &str, url: &str) -> Result<Release> {
    let resp = client
        .get(url)
        .header("User-Agent", user_agent)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("fetching {url} returned {status}");
    }
    resp.json().await.context("parsing release metadata")
}

/// Versioning scheme: stable releases are `x.y` (two components, e.g.
/// `v0.9`), development builds are `x.y.z-beta` (three components plus the
/// `-beta` suffix for GitHub clarity, e.g. `v0.9.1-beta`). The suffix is
/// display-only: channel membership is decided by component COUNT, so a
/// missing suffix can never promote a beta to stable.
///
/// Legacy tags (`v0.8.8-beta`, two components + suffix, from before the
/// scheme change) classify as beta via the suffix rule below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Beta,
}

/// Classify a tag into its release channel. Strips a leading `v`, then:
/// a `-beta`/`-rc`/`-alpha` suffix means beta (covers both the new
/// `x.y.z-beta` shape and legacy `x.y-beta` tags); no suffix with exactly
/// two dot-separated components means stable. Anything else (bare
/// three-component tags, unknown suffixes, unparseable shapes) is neither —
/// failing closed so an unknown shape can never leak across channels.
pub fn channel_of(tag: &str) -> Option<Channel> {
    let t = tag.trim_start_matches('v');
    let (core, suffix) = match t.split_once('-') {
        Some((c, s)) => (c, Some(s)),
        None => (t, None),
    };
    if let Some(s) = suffix {
        let s = s.to_ascii_lowercase();
        if s.starts_with("beta") || s.starts_with("rc") || s.starts_with("alpha") {
            return Some(Channel::Beta);
        }
        // Unknown suffix: not ours, exclude from both channels — whatever
        // the component count. (A bare three-component tag without suffix
        // is likewise NOT ours: the scheme always ships -beta on
        // development builds. Failing closed keeps an unknown shape from
        // ever leaking across channels in either direction.)
        return None;
    }
    match core.split('.').count() {
        // Two components, no suffix: a stable release (x.y).
        2 => Some(Channel::Stable),
        // Three components WITHOUT suffix: NOT ours (development builds
        // always ship -beta). Fail closed — unclassified, never leaks.
        _ => None,
    }
}

async fn fetch_latest(
    client: &reqwest::Client,
    user_agent: &str,
    include_beta: bool,
) -> Result<Release> {
    if !include_beta {
        return fetch_release(client, user_agent, &releases_url()).await;
    }
    let resp = client
        .get(releases_list_url())
        .header("User-Agent", user_agent)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("fetching releases")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("fetching releases returned {status}");
    }
    let releases: Vec<Release> = resp.json().await.context("parsing release metadata")?;
    // Beta channel: newest release whose tag classifies as beta. The
    // releases API returns newest-first, so the first beta-classified,
    // non-draft release wins. (Drafts excluded; GitHub prerelease flags
    // ignored — the tag shape is the source of truth, so a mis-flagged
    // release can't cross channels.)
    releases
        .into_iter()
        .filter(|r| !r.draft)
        .find(|r| channel_of(&r.tag_name) == Some(Channel::Beta))
        .context("no beta releases found")
}

pub async fn latest_version(
    client: &reqwest::Client,
    user_agent: &str,
    include_beta: bool,
) -> Result<String> {
    Ok(fetch_latest(client, user_agent, include_beta)
        .await?
        .tag_name)
}

/// Asset arch in the release naming scheme (Go-style GOARCH, matching
/// scripts/build-release.sh): amd64/arm64/arm/386 — NOT Rust target_arch
/// (x86_64/aarch64), which names no published asset.
pub fn asset_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        "x86" => "386",
        other => other,
    }
}

fn asset_name() -> String {
    format!("keep-at_linux_{}.tar.gz", asset_arch())
}

/// Download the matching asset and replace current_exe atomically.
pub async fn apply(
    client: &reqwest::Client,
    user_agent: &str,
    current_exe: &std::path::Path,
    include_beta: bool,
) -> Result<String> {
    let rel = fetch_latest(client, user_agent, include_beta).await?;
    let want = asset_name();
    let url = rel
        .assets
        .iter()
        .find(|a| a.name == want)
        .map(|a| a.browser_download_url.clone())
        .with_context(|| format!("no release asset named {want} in {}", rel.tag_name))?;

    let resp = client
        .get(&url)
        .header("User-Agent", user_agent)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("downloading {url} returned {status}");
    }
    let body = resp.bytes().await.context("reading download")?;

    let binary =
        extract_binary(&body).with_context(|| format!("extracting keep-at binary from {url}"))?;
    replace_executable(current_exe, &binary)?;
    Ok(rel.tag_name)
}

fn extract_binary(tar_gz: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let gz = GzDecoder::new(tar_gz);
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries().context("reading tar archive")? {
        let mut entry = entry?;
        let path = entry.path().context("reading tar entry path")?.into_owned();
        let base = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if base == "keep-at" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    anyhow::bail!("archive did not contain a keep-at binary")
}

fn replace_executable(target: &std::path::Path, new_binary: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(target)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o755);
    let dir = target.parent().context("executable has no parent dir")?;
    let tmp = dir.join(format!(".keep-at-update-{}", std::process::id()));
    std::fs::write(&tmp, new_binary).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, target).with_context(|| format!("replacing {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_arch_matches_release_naming() {
        // Release assets use Go-style GOARCH (build-release.sh). A mismatch
        // here breaks self-update on every platform with zero test signal
        // otherwise (found live: x86_64 vs amd64 on the stable channel).
        assert_eq!(
            asset_arch(),
            match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                "arm" => "arm",
                "x86" => "386",
                other => other,
            }
        );
        assert!(asset_name().starts_with("keep-at_linux_"));
        assert!(asset_name().ends_with(".tar.gz"));
    }

    #[test]
    fn channel_classification() {
        use Channel::{Beta, Stable};
        // New scheme: two components stable, three + suffix beta.
        assert_eq!(channel_of("v0.9"), Some(Stable));
        assert_eq!(channel_of("0.10"), Some(Stable));
        assert_eq!(channel_of("v1.0"), Some(Stable));
        assert_eq!(channel_of("v0.9.1-beta"), Some(Beta));
        assert_eq!(channel_of("v0.10.2-beta"), Some(Beta));
        assert_eq!(channel_of("v1.0.1-beta"), Some(Beta));
        // Legacy tags (two components + suffix, pre-scheme-change).
        assert_eq!(channel_of("v0.8.8-beta"), Some(Beta));
        assert_eq!(channel_of("v0.7.2-rc"), Some(Beta));
        // Unknown shapes classify as neither (never leak across channels):
        // a bare three-component tag or an unknown suffix is NOT ours, so
        // both stay unclassified rather than risk promoting them.
        assert_eq!(channel_of("v0.9.1"), None);
        assert_eq!(channel_of("v0.9-experimental"), None);
        assert_eq!(channel_of("v0.9.1-next"), None);
        assert_eq!(channel_of("nightly"), None);
        assert_eq!(channel_of(""), None);
    }
}
