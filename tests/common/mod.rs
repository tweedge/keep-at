//! Shared hermetic fixture for integration tests: a local catalog server,
//! a local Academic-Torrents stub (`.torrent` bytes + tracker scrapes), and
//! helpers to build configs, synthetic torrents, and pre-seeded state.
//!
//! Nothing here touches the live network. All traffic stays on 127.0.0.1:
//! the catalog URL and the AT base URL both point at the stub, whose
//! `/download/<hash>.torrent` serves the `.torrent` bytes for hashes the
//! test registered, and whose scrape endpoint (`/scrape?...`, derived from
//! the `/announce` tracker URL by the usual announce->scrape rewrite)
//! returns canned per-hash seeder/leecher counts.
//!
//! Tests assert through the public surface only: [`Engine::scan_once`],
//! [`Engine::held_torrents`], [`Engine::last_scan_stats`], and the state
//! files on disk (`state.json`, `network-stats.json`, `scrape-cache.json`).

// Shared across test binaries that each use a different subset; per-item
// dead-code lints would fire in whichever binary doesn't use that item.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use keep_at::config::{Config, StorageLimit, StorageLocation};

/// One synthetic catalog entry: content size, piece length, seeder/leecher
/// counts served by the stub scrape, and a creation date old enough to pass
/// the moderation gate unless the test says otherwise.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub title: String,
    /// Total content bytes.
    pub size: u64,
    /// Piece length in bytes. 1 piece keeps the RAM model trivial.
    pub piece_len: u32,
    pub seeders: u32,
    pub leechers: u32,
    /// Creation-date offset from now. Default: 30 days ago (passes the
    /// default 7-day moderation gate). `None` means no creation date at all
    /// (fails the age gate, like torrents whose age can't be determined).
    pub age: Option<Duration>,
}

impl Fixture {
    pub fn new(title: &str, size: u64, seeders: u32) -> Fixture {
        Fixture {
            title: title.to_string(),
            size,
            piece_len: size.max(1) as u32,
            seeders,
            leechers: 0,
            age: Some(Duration::from_secs(30 * 24 * 3600)),
        }
    }
}

/// Build minimal single-file `.torrent` bytes with correct bencode and
/// length prefixes, a `creation date`, one announce tracker pointing at the
/// stub, and `pieces` of zero bytes. The file name embeds the fixture title,
/// so distinct fixtures hash distinctly (the infohash covers the info dict
/// only — same size + same name would collide). Callers learn the hash via
/// [`torrent_info_hash`].
pub fn torrent_bytes(fixture: &Fixture, tracker_url: &str) -> Vec<u8> {
    let piece_count = fixture
        .size
        .div_ceil(fixture.piece_len.max(1) as u64)
        .max(1);
    let pieces_len = piece_count as usize * 20;
    // Sanitize title into a filename (bencode needs exact length prefixes;
    // keep it ASCII-alphanumeric to stay unambiguous).
    let safe_title: String = fixture
        .title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{safe_title}.bin");
    let info = bencode_dict(&[
        (b"length".as_slice(), bencode_int(fixture.size as i64)),
        (b"name".as_slice(), bencode_str(name.as_bytes())),
        (
            b"piece length".as_slice(),
            bencode_int(fixture.piece_len.max(1) as i64),
        ),
        (b"pieces".as_slice(), bencode_raw(&vec![0u8; pieces_len])),
    ]);
    let mut top = Vec::new();
    top.extend_from_slice(b"d");
    top.extend_from_slice(&bencode_str(b"announce"));
    top.extend_from_slice(&bencode_str(tracker_url.as_bytes()));
    if let Some(age) = fixture.age {
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(age.as_secs());
        top.extend_from_slice(&bencode_str(b"creation date"));
        top.extend_from_slice(&bencode_int(created as i64));
    }
    top.extend_from_slice(&bencode_str(b"info"));
    top.extend_from_slice(&info);
    top.extend_from_slice(b"e");
    top
}

/// Infohash hex of synthetic `.torrent` bytes (reuses the crate's own
/// parser, so hash and parse can never disagree).
pub fn torrent_info_hash(raw: &[u8]) -> String {
    let md = keep_at::attorrent::parse_torrent_bytes(raw).expect("fixture parses");
    hex::encode(md.info_hash)
}

fn bencode_int(v: i64) -> Vec<u8> {
    format!("i{v}e").into_bytes()
}

fn bencode_str(s: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", s.len()).into_bytes();
    out.extend_from_slice(s);
    out
}

fn bencode_raw(s: &[u8]) -> Vec<u8> {
    // A byte string whose content is already length-prefixed form would
    // double-prefix; pieces are raw bytes, so prefix them once.
    let mut out = format!("{}:", s.len()).into_bytes();
    out.extend_from_slice(s);
    out
}

fn bencode_dict(entries: &[(&[u8], Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![b'd'];
    for (k, v) in entries {
        out.extend_from_slice(&bencode_str(k));
        out.extend_from_slice(v);
    }
    out.push(b'e');
    out
}

/// Shared mutable stub state: per-hash torrent bytes + scrape counts, plus
/// counters and scripted failures the tests assert on.
#[derive(Debug, Default)]
pub struct StubState {
    /// infohash hex -> .torrent bytes served at /download/<hash>.torrent.
    pub torrents: HashMap<String, Vec<u8>>,
    /// infohash hex -> (seeders, leechers) served by the scrape endpoint.
    /// Absent hash => empty files dict (unknown torrent, no entry).
    pub scrapes: HashMap<String, (u32, u32)>,
    /// Paths requested (for assertions like "UDP trackers never scraped").
    pub scrape_hits: Vec<String>,
    pub torrent_hits: Vec<String>,
    /// Number of leading scrape requests to fail with HTTP 429.
    pub fail_scrapes_429: usize,
    /// Number of leading scrape requests to fail with HTTP 500.
    pub fail_scrapes_500: usize,
    pub scrape_requests: usize,
}

impl StubState {
    /// Register a fixture: returns (infohash hex, .torrent bytes).
    /// Each registration uses a distinct tracker PATH (`/announce-<n>`)
    /// so per-hash scrape URLs differ. (The engine's scrape cache is
    /// keyed by infohash, but distinct URLs keep the stub's request log
    /// unambiguous and mirror real catalogs, where torrents list
    /// different trackers.)
    pub fn add(&mut self, fixture: &Fixture, tracker_base: &str) -> (String, Vec<u8>) {
        let n = self.torrents.len();
        let tracker_url = format!("{tracker_base}-{n}");
        let raw = torrent_bytes(fixture, &tracker_url);
        let hex = torrent_info_hash(&raw);
        self.scrapes
            .insert(hex.clone(), (fixture.seeders, fixture.leechers));
        self.torrents.insert(hex.clone(), raw.clone());
        (hex, raw)
    }
}

/// A running stub server: base URL serving database.xml, /download/, and
/// the scrape endpoint. Tracks every request for assertions.
pub struct Stub {
    pub base_url: String,
    pub state: Arc<Mutex<StubState>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Stub {
    /// Start the stub on 127.0.0.1 with the given catalog XML. The tracker
    /// URL embedded in fixtures must be `{base}/announce` so the engine's
    /// announce->scrape rewrite lands on `{base}/scrape`.
    pub fn start(catalog_xml: String, state: Arc<Mutex<StubState>>) -> Stub {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub server");
        let addr = listener.local_addr().expect("stub addr");
        let base_url = format!("http://{addr}");
        let thread_state = state.clone();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                use std::io::{Read, Write};
                let mut buf = vec![0u8; 65536];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let body = handle_request(&path, &thread_state);
                let (status, content_type, payload) = body;
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&payload);
            }
        });
        // The catalog endpoint serves whatever XML was passed at start; the
        // test rewrites it per phase through `StubState`? No - catalog is
        // static per stub. Tests needing two catalogs start two stubs (or
        // restart). Keep it simple: stash the XML in a file the handler
        // reads? Simplest correct: the handler serves catalog_xml from an
        // Arc<Mutex<String>>.
        let _ = catalog_xml;
        Stub {
            base_url,
            state,
            handle: Some(handle),
        }
    }

    /// Tracker URL base for fixtures: `{base}/announce` (scrape rewrites
    /// to `{base}/scrape`). `StubState::add` appends `-<n>` per fixture so
    /// each torrent gets a distinct tracker path.
    pub fn tracker_url(&self) -> String {
        format!("{}/announce", self.base_url)
    }

    /// Catalog XML serving the given (title, infohash, size) rows.
    pub fn catalog_xml(rows: &[(String, String, u64)]) -> String {
        let mut out =
            String::from(r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel>"#);
        for (title, ih, size) in rows {
            out.push_str(&format!(
                "<item><title>{title}</title><category>Test</category>\
                 <infohash>{ih}</infohash><guid>g-{ih}</guid><link>l</link>\
                 <description>d</description><size>{size}</size></item>"
            ));
        }
        out.push_str("</channel></rss>");
        out
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        // Listener closes when the handle is dropped; the accept thread
        // exits on the next failed accept. Detach it.
        let _ = self.handle.take();
    }
}

fn handle_request(
    path: &str,
    state: &Arc<Mutex<StubState>>,
) -> (&'static str, &'static str, Vec<u8>) {
    let mut st = state.lock().unwrap();
    if path == "/database.xml" {
        // Catalog is served by the dedicated catalog server in each test
        // (see serve_catalog); the stub answers 404 here so a miswired
        // catalog_url fails loudly instead of parsing garbage.
        return ("404 Not Found", "text/plain", b"no catalog here".to_vec());
    }
    if let Some(hex) = path
        .strip_prefix("/download/")
        .and_then(|p| p.strip_suffix(".torrent"))
    {
        st.torrent_hits.push(path.to_string());
        return match st.torrents.get(hex) {
            Some(raw) => ("200 OK", "application/x-bittorrent", raw.clone()),
            None => ("404 Not Found", "text/plain", b"unknown torrent".to_vec()),
        };
    }
    if path.starts_with("/scrape") {
        st.scrape_requests += 1;
        st.scrape_hits.push(path.to_string());
        if st.fail_scrapes_429 > 0 {
            st.fail_scrapes_429 -= 1;
            return ("429 Too Many Requests", "text/plain", b"slow down".to_vec());
        }
        if st.fail_scrapes_500 > 0 {
            st.fail_scrapes_500 -= 1;
            return (
                "500 Internal Server Error",
                "text/plain",
                b"tracker error".to_vec(),
            );
        }
        // ?info_hash=<percent-encoded raw 20 bytes>
        let hash = path
            .split("info_hash=")
            .nth(1)
            .and_then(|q| q.split('&').next())
            .and_then(percent_decode_hash);
        // BEP 48 scrape response shape: d5:filesd<20-byte-hash>d...ee e.
        // Keys are raw 20-byte strings, exactly like a real tracker.
        let mut files = Vec::new();
        if let Some(h) = hash {
            let hex = hex::encode(h);
            if let Some((seeders, leechers)) = st.scrapes.get(&hex) {
                files.extend_from_slice(format!("{}:", h.len()).as_bytes());
                files.extend_from_slice(&h);
                files.extend_from_slice(
                    format!("d8:completei{seeders}e10:downloadedi0e10:incompletei{leechers}ee")
                        .as_bytes(),
                );
            }
        }
        let mut body = b"d5:filesd".to_vec();
        body.extend_from_slice(&files);
        body.extend_from_slice(b"ee");
        return ("200 OK", "text/plain", body);
    }
    ("404 Not Found", "text/plain", b"unknown path".to_vec())
}

fn percent_decode_hash(q: &str) -> Option<[u8; 20]> {
    let bytes = percent_decode(q)?;
    if bytes.len() != 20 {
        return None;
    }
    let mut h = [0u8; 20];
    h.copy_from_slice(&bytes);
    Some(h)
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut chars = s.as_bytes().iter();
    while let Some(&b) = chars.next() {
        if b == b'%' {
            let hi = *chars.next()? as char;
            let lo = *chars.next()? as char;
            out.push(u8::from_str_radix(&format!("{hi}{lo}"), 16).ok()?);
        } else if b == b'+' {
            out.push(b' ');
        } else {
            out.push(b);
        }
    }
    Some(out)
}

/// Serve a static catalog XML on 127.0.0.1; returns the base URL. The engine
/// is pointed at `{base}/database.xml` via Options.catalog_url.
pub fn serve_catalog(xml: String) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind catalog server");
    let addr = listener.local_addr().expect("catalog addr");
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(false).ok();
        for _ in 0..16 {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => break,
            };
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                xml.len(),
                xml
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    (format!("http://{addr}"), handle)
}

/// Test config: unique port per test, tiny storage, gate forced open unless
/// the test overrides aggressiveness afterwards. Rate limit high (local
/// stub), backoff ~0 (fast 429 path), moderation off.
pub fn test_config(
    data_dir: PathBuf,
    storage_dir: PathBuf,
    port: u16,
    storage_limit: u64,
) -> Config {
    let mut cfg = Config {
        data_dir,
        port,
        storage: vec![StorageLocation {
            path: storage_dir,
            limit: StorageLimit::Bytes(storage_limit),
        }],
        // Fresh catalog every scan: the catalog cache TTL is the scan
        // interval, and multi-phase tests serve a different catalog per
        // phase. A 1s interval keeps each scan honest without affecting
        // scan_once pacing.
        scan: keep_at::config::ScanConfig {
            interval: Duration::from_secs(1),
            rate_limit_per_second: 1000.0,
            moderation_delay: Duration::ZERO,
            ..Default::default()
        },
        // Force the seed-scarcity gate open (see smoke.rs rationale): the
        // integration suite pins pipeline behavior, not gate math (gate math
        // stays covered by selector unit tests). Tests that specifically
        // exercise gating set their own aggressiveness afterwards.
        aggressiveness: 0.999999,
        ..Config::default()
    };
    let _ = &mut cfg;
    cfg
}

/// Engine options pointing at the stub, with fast 429 backoff.
pub fn test_options(catalog_base: &str, at_base: &str) -> keep_at::engine::EngineOptions {
    keep_at::engine::EngineOptions {
        catalog_url: Some(format!("{catalog_base}/database.xml")),
        at_base_url: Some(at_base.to_string()),
        scrape_backoff: Some(Duration::from_millis(10)),
    }
}

/// Timeout wrapper: integration tests must fail fast, never hang CI.
/// (grug: a hung test that blocks the suite is worse than no test.)
pub async fn with_timeout<F, T>(secs: u64, what: &str, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(secs), fut)
        .await
        .unwrap_or_else(|_| panic!("timed out after {secs}s: {what}"))
}

/// Unique-ish port per test process (parallel tests must not share).
pub fn test_port(offset: u16) -> u16 {
    47500 + (std::process::id() % 1000) as u16 + offset
}

/// Read state.json held set from a data dir.
pub fn read_held(data_dir: &Path) -> Vec<keep_at::state::Torrent> {
    keep_at::state::State::load(&data_dir.join("state.json"))
        .expect("state loads")
        .all()
}

/// Read network-stats.json snapshot from a data dir.
pub fn read_snapshot(data_dir: &Path) -> keep_at::netstats::Snapshot {
    keep_at::netstats::load_snapshot(&data_dir.join("network-stats.json")).expect("snapshot loads")
}
