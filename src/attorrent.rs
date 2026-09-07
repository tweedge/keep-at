//! Per-torrent Academic Torrents access: .torrent fetch/parse and
//! tracker scrapes.

use anyhow::{Context, Result};
use librqbit_core::torrent_metainfo;

#[derive(Debug, Clone, Copy, Default)]
pub struct SwarmCounts {
    pub seeders: u32,
    pub leechers: u32,
    pub completed: u32,
}

#[derive(Debug, Clone)]
pub struct TorrentMeta {
    pub info_hash: [u8; 20],
    pub trackers: Vec<String>,
    /// Torrent creation time, if present. Zero/non-existent => None (treated
    /// as not yet age-eligible by the caller).
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub total_length: u64,
    /// Number of pieces. Drives the RAM model: rqbit's per-torrent
    /// bookkeeping scales ~32 B/piece (measured), so piece count - not byte
    /// size - is what makes one torrent cost more RAM than another.
    pub piece_count: u32,
    pub name: String,
}

/// TorrentMeta carries no bulk data: fetchers write the exact response body
/// to the torrent-cache file, and the add path re-reads it from disk only at
/// the moment rqbit needs it. So scans hold ~100 B/candidate in memory, not
/// the 64 KiB average .torrent file (measured 182 MiB scan peak eliminated).
pub struct Fetcher {
    pub base_url: String,
    pub user_agent: String,
    pub client: reqwest::Client,
}

impl Fetcher {
    pub async fn fetch_torrent(
        &self,
        info_hash_hex: &str,
        cache_path: Option<&std::path::Path>,
    ) -> Result<(TorrentMeta, Vec<u8>)> {
        let url = format!("{}/download/{info_hash_hex}.torrent", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("fetching {url} returned {status}");
        }
        let body = resp
            .bytes()
            .await
            .with_context(|| format!("reading {url}"))?;
        let md = parse_torrent_bytes(&body)?;
        if let Some(path) = cache_path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let mut tmp_os = path.as_os_str().to_owned();
            tmp_os.push(".tmp");
            let tmp = std::path::PathBuf::from(tmp_os);
            if std::fs::write(&tmp, &body).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
        Ok((md, body.to_vec()))
    }
}

// Truncate a raw tracker byte-string at the end of the URL. The rqbit
// bencode parser returns the raw span, which for some AT .torrent files
// leaks trailing bencode into the string (e.g. "...announce.php13:...").
// Cut at the first interior "<digits>:" run whose prefix is a valid URL
// and whose digit run is NOT part of the URL's own authority section
// (host, or host:port). A port like ":1337" is kept, as is a dotted-quad
// host ("127.0.0.1:42315": the run before the port colon is preceded by a
// dot/digit inside the authority, so it is never treated as a leak).
fn trunc_url(raw: &[u8]) -> &[u8] {
    let mut end = raw
        .iter()
        .position(|&b| !(0x21..=0x7e).contains(&b))
        .unwrap_or(raw.len());
    // Authority section = bytes between "://" and the next '/'. Digit runs
    // inside it (octets, ports) are URL structure, never bencode leaks.
    let auth_end = raw
        .windows(3)
        .position(|w| w == b"://")
        .map(|p| {
            raw[p + 3..]
                .iter()
                .position(|&b| b == b'/')
                .map(|q| p + 3 + q)
                .unwrap_or(raw.len())
        })
        .unwrap_or(0);
    let mut i = 0;
    while i < end {
        if raw[i].is_ascii_digit() {
            let mut j = i;
            while j < end && raw[j].is_ascii_digit() {
                j += 1;
            }
            if j < end && raw[j] == b':' && j > i && i >= auth_end {
                if is_url(&raw[..i]) {
                    end = i;
                    break;
                }
                i = j + 1;
                continue;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    let mut e = end;
    loop {
        if e == 0 {
            return &raw[..0];
        }
        if is_url(&raw[..e]) {
            return &raw[..e];
        }
        match raw[..e.saturating_sub(1)].iter().rposition(|&b| b == b'/') {
            Some(p) => e = p,
            None => return &raw[..0],
        }
    }
}
fn is_url(s: &[u8]) -> bool {
    let mut i = 0;
    while i < s.len()
        && (s[i].is_ascii_alphanumeric() || s[i] == b'+' || s[i] == b'-' || s[i] == b'.')
    {
        i += 1;
    }
    if i == 0 || s.get(i..i + 3) != Some(b"://") {
        return false;
    }
    s.len() > i + 3
}

/// Parse raw .torrent bytes into the metadata keep-at needs.
pub fn parse_torrent_bytes(body: &[u8]) -> Result<TorrentMeta> {
    let meta = torrent_metainfo::torrent_from_bytes(body).context("parsing torrent file")?;
    let info_hash: [u8; 20] = meta.info_hash.0;

    // Canonical tracker list: rqbit's TorrentMetaV1::iter_announce yields
    // announce-list tiers when present, else announce - exactly what rqbit
    // itself announces to. trunc_url stays as defense for malformed files.
    let mut trackers: Vec<String> = Vec::new();
    for raw in meta.iter_announce() {
        let bytes: &[u8] = raw.as_ref();
        let s = String::from_utf8_lossy(trunc_url(bytes)).into_owned();
        if !s.is_empty() && !trackers.contains(&s) {
            trackers.push(s);
        }
    }

    let created_at = meta
        .creation_date
        .and_then(|secs| chrono::DateTime::from_timestamp(secs as i64, 0));

    let validated = meta
        .info
        .data
        .clone()
        .validate()
        .context("validating torrent info")?;
    let total_length = validated.lengths().total_length();
    let piece_count = validated.lengths().total_pieces();
    let name: String = validated
        .name()
        .map(|n| n.into_owned())
        .filter(|n: &String| !n.is_empty())
        .unwrap_or_else(|| hex::encode(info_hash));

    Ok(TorrentMeta {
        info_hash,
        trackers,
        created_at,
        total_length,
        piece_count,
        name,
    })
}

/// Derive the BEP 48 scrape URL from an announce URL: in the final path
/// segment, replace "announce" with "scrape".
pub fn derive_http_scrape_url(announce_url: &str) -> Option<String> {
    let slash = announce_url.rfind('/')?;
    let (base, segment) = announce_url.split_at(slash + 1);
    if !segment.contains("announce") {
        return None;
    }
    Some(format!(
        "{}{}",
        base,
        segment.replacen("announce", "scrape", 1)
    ))
}

/// Scrape one HTTP(S) tracker for a single hash (AT's tracker does not
/// support BEP 48 multi-hash scrapes - it returns data only for the last
/// hash - so keep-at scrapes one hash per request).
pub async fn scrape_http(
    client: &reqwest::Client,
    user_agent: &str,
    announce_url: &str,
    info_hash: &[u8; 20],
) -> Result<SwarmCounts> {
    let scrape_url = derive_http_scrape_url(announce_url)
        .context("tracker does not follow the announce/scrape URL convention")?;

    // info_hash must be percent-encoded raw bytes.
    let mut encoded = String::with_capacity(60);
    for b in info_hash {
        encoded.push_str(&format!("%{b:02X}"));
    }
    let url = format!("{scrape_url}?info_hash={encoded}");
    let resp = client
        .get(&url)
        .header("User-Agent", user_agent)
        .send()
        .await
        .with_context(|| format!("scrape request to {scrape_url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("scrape {scrape_url} returned {status}");
    }
    let body = resp.bytes().await.context("reading scrape response")?;
    decode_scrape_response(&body, info_hash)
}

fn decode_scrape_response(body: &[u8], info_hash: &[u8; 20]) -> Result<SwarmCounts> {
    // Hand-rolled parse over BencodeValue: keys are raw 20-byte strings.
    let v: librqbit_bencode::BencodeValue<librqbit_bencode::ByteBufOwned> =
        librqbit_bencode::from_bytes(body)
            .map_err(|e| anyhow::anyhow!("decoding scrape response: {e:?}"))?;
    let dict = match &v {
        librqbit_bencode::BencodeValue::Dict(d) => d,
        _ => anyhow::bail!("scrape response is not a dict"),
    };
    let files = dict
        .iter()
        .find(|(k, _)| k.as_ref() == b"files")
        .map(|(_, v)| v);
    let files_dict = match files {
        Some(librqbit_bencode::BencodeValue::Dict(d)) => d,
        _ => anyhow::bail!("scrape response has no files dict"),
    };
    for (k, f) in files_dict {
        if k.as_ref() != info_hash {
            continue;
        }
        let fdict = match f {
            librqbit_bencode::BencodeValue::Dict(d) => d,
            _ => continue,
        };
        let get = |name: &[u8]| -> u32 {
            fdict
                .iter()
                .find(|(k, _)| k.as_ref() == name)
                .and_then(|(_, v)| match v {
                    librqbit_bencode::BencodeValue::Integer(i) => u32::try_from(*i).ok(),
                    _ => None,
                })
                .unwrap_or(0)
        };
        return Ok(SwarmCounts {
            seeders: get(b"complete"),
            completed: get(b"downloaded"),
            leechers: get(b"incomplete"),
        });
    }
    anyhow::bail!("scrape response had no entry for the requested hash")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_truncation() {
        let t = |b: &[u8]| String::from_utf8_lossy(trunc_url(b)).into_owned();
        // bencode leak after announce URL
        assert_eq!(
            t(b"https://academictorrents.com/announce.php13:announce-listll41:https://x"),
            "https://academictorrents.com/announce.php"
        );
        // real port preserved
        assert_eq!(
            t(b"udp://tracker.opentrackr.org:1337/announce"),
            "udp://tracker.opentrackr.org:1337/announce"
        );
        // clean URL untouched
        assert_eq!(
            t(b"https://academictorrents.com/announce.php"),
            "https://academictorrents.com/announce.php"
        );
        // webseed-ish URL with interior digits untouched
        assert_eq!(
            t(b"http://jmlr.org/papers/v14/x13a.pdf"),
            "http://jmlr.org/papers/v14/x13a.pdf"
        );
        // dotted-quad host + port untouched (regression: the old heuristic
        // cut "http://127.0.0.1" at the port colon, breaking every
        // 127.0.0.1-based test stub and any real dotted-quad tracker).
        assert_eq!(
            t(b"http://127.0.0.1:42315/announce"),
            "http://127.0.0.1:42315/announce"
        );
        // leak after a dotted-quad URL still cut
        assert_eq!(
            t(b"http://127.0.0.1:42315/announce13:announce-listll41:http://x"),
            "http://127.0.0.1:42315/announce"
        );
    }

    #[test]
    fn scrape_url_derivation() {
        assert_eq!(
            derive_http_scrape_url("https://academictorrents.com/announce.php?passkey=x"),
            Some("https://academictorrents.com/scrape.php?passkey=x".to_string())
        );
        assert_eq!(
            derive_http_scrape_url("udp://tracker:80/announce"),
            Some("udp://tracker:80/scrape".to_string())
        );
        assert_eq!(derive_http_scrape_url("https://example.com/track"), None);
    }
}
