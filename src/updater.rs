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
    releases
        .into_iter()
        .find(|r| !r.draft)
        .context("no releases found")
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
}
