//! Academic Torrents catalog client: fetch + parse database.xml.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_URL: &str = "https://academictorrents.com/database.xml";

#[derive(Debug, Clone)]
pub struct Item {
    pub title: String,
    pub category: String,
    pub info_hash: [u8; 20],
    pub guid: String,
    pub link: String,
    pub description: String,
    pub size_bytes: u64,
}

impl Item {
    pub fn info_hash_hex(&self) -> String {
        hex::encode(self.info_hash)
    }
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub items: Vec<Item>,
    pub fetched_at: SystemTime,
}

#[derive(Debug, Deserialize)]
struct Rss {
    channel: Channel,
}

#[derive(Debug, Deserialize)]
struct Channel {
    #[serde(default, rename = "item")]
    items: Vec<RssItem>,
}

#[derive(Debug, Deserialize)]
struct RssItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    infohash: String,
    #[serde(default)]
    guid: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    size: u64,
}

/// Parse a database.xml document. Malformed entries (bad infohash) are
/// skipped rather than failing the whole parse.
pub fn parse(data: &[u8]) -> Result<Catalog> {
    let doc: Rss = quick_xml::de::from_reader(data).context("decoding database.xml")?;
    let mut items = Vec::with_capacity(doc.channel.items.len());
    for raw in doc.channel.items {
        let hash = match hex::decode(raw.infohash.trim()) {
            Ok(b) if b.len() == 20 => {
                let mut h = [0u8; 20];
                h.copy_from_slice(&b);
                h
            }
            _ => continue,
        };
        items.push(Item {
            title: raw.title,
            category: raw.category,
            info_hash: hash,
            guid: raw.guid,
            link: raw.link,
            description: raw.description,
            size_bytes: raw.size,
        });
    }
    Ok(Catalog {
        items,
        fetched_at: SystemTime::now(),
    })
}

/// Download raw database.xml bytes.
pub async fn fetch_raw(client: &reqwest::Client, url: &str, user_agent: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .header("User-Agent", user_agent)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("fetching {url} returned {status}");
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .with_context(|| "reading database.xml")
}

/// Fetcher caches database.xml on disk and only refetches when stale.
pub struct Fetcher {
    pub cache_path: PathBuf,
    pub url: String,
    pub user_agent: String,
    pub client: reqwest::Client,
}

impl Fetcher {
    /// Load the catalog, refetching when the cache is missing or older than
    /// max_age. On fetch failure with a usable cache, returns the stale
    /// cache (caller decides whether that's fatal).
    pub async fn load(&self, max_age: Duration) -> Result<(Catalog, bool)> {
        let stale = cache_age(&self.cache_path)
            .map(|a| a > max_age)
            .unwrap_or(true);
        if !stale {
            if let Ok(data) = std::fs::read(&self.cache_path) {
                if let Ok(cat) = parse(&data) {
                    return Ok((cat, false));
                }
            }
        }
        match fetch_raw(&self.client, &self.url, &self.user_agent).await {
            Ok(data) => {
                let cat = parse(&data)?;
                let _ = atomic_cache_write(&self.cache_path, &data);
                Ok((cat, true))
            }
            Err(e) => {
                if let Ok(data) = std::fs::read(&self.cache_path) {
                    if let Ok(cat) = parse(&data) {
                        tracing::warn!(
                            "catalog refresh failed, continuing with stale cache: {e:#}"
                        );
                        return Ok((cat, false));
                    }
                }
                Err(e)
            }
        }
    }
}

fn cache_age(path: &Path) -> Option<Duration> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(m).ok()
}

fn atomic_cache_write(path: &Path, data: &[u8]) -> Result<()> {
    // World-readable like every other cache/snapshot (status and
    // hosted-torrents read as any user).
    crate::config::atomic_write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<rss><channel>
<item><title>Test Dataset</title><category>papers</category><infohash>da39a3ee5e6b4b0d3255bfef95601890afd80709</infohash><guid>g1</guid><link>http://x</link><description>desc</description><size>12345</size></item>
<item><title>Bad Hash</title><infohash>zzz</infohash><size>1</size></item>
</channel></rss>"#;

    #[test]
    fn parse_skips_bad_rows() {
        let cat = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(cat.items.len(), 1);
        assert_eq!(cat.items[0].title, "Test Dataset");
        assert_eq!(cat.items[0].size_bytes, 12345);
        assert_eq!(
            cat.items[0].info_hash_hex(),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }
}
