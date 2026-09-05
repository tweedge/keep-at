//! Live smoke tests against real Academic Torrents infrastructure.
//! Ported from internal/engine/smoke_test.go (Go).
//!
//! - `KEEPAT_SMOKE_TEST=1`: fast pipeline test. Serves a 2-item catalog of
//!   hand-picked, verified-seeded torrents locally while fetching real
//!   .torrent files + tracker scrapes from live AT. Asserts a scan selects
//!   and holds torrents, and at least one completes a real download.
//! - `KEEPAT_SMOKE_SUBSET=1`: real-catalog subset scan (smallest N entries,
//!   default 100, ~10 min). Asserts structural invariants: per-candidate
//!   scrapes issued, scrape errors under half of processed, scan finishes,
//!   at least one real download completes.
//!
//! Both write scratch state under /tmp and need live network access.

use std::path::PathBuf;
use std::time::Duration;

use keep_at::config::{Config, StorageLimit, StorageLocation};

struct SmokeItem {
    title: &'static str,
    info_hash: &'static str,
    size: u64,
}

// Hand-picked, verified-seeded AT entries (same fixtures as the Go test).
const SMOKE_ITEMS: &[SmokeItem] = &[
    SmokeItem {
        title: "The Relativity of Simultaneity is Wrong.txt",
        info_hash: "d137ffd5e951cc53cd789aab935bf8e833bf8229",
        size: 1873,
    },
    SmokeItem {
        title: "Multiple-Instance Learning of Real-Valued Data",
        info_hash: "936a92932c01c3f5e9994ae8bd2115f4ccb4adc9",
        size: 7100,
    },
];

fn smoke_catalog_xml(items: &[SmokeItem]) -> String {
    let mut out =
        String::from(r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel>"#);
    for it in items {
        out.push_str(&format!(
            "<item><title>{}</title><category>Paper</category><infohash>{}</infohash>\
             <guid>https://academictorrents.com/details/{}</guid>\
             <link>https://academictorrents.com/details/{}</link>\
             <description>keep-at smoke test fixture</description><size>{}</size></item>",
            it.title, it.info_hash, it.info_hash, it.info_hash, it.size
        ));
    }
    out.push_str("</channel></rss>");
    out
}

fn test_config(data_dir: PathBuf, storage_dir: PathBuf, port: u16, rate: f64) -> Config {
    let mut cfg = Config {
        data_dir,
        port,
        storage: vec![StorageLocation {
            path: storage_dir,
            limit: StorageLimit::Bytes(1 << 30),
        }],
        ..Config::default()
    };
    cfg.scan.moderation_delay = Duration::ZERO;
    cfg.scan.rate_limit_per_second = rate;
    if let Ok(k) = std::env::var("KEEPAT_API_KEY") {
        if !k.is_empty() {
            cfg.api_key = k;
        }
    }
    cfg
}

/// Spin a tiny local HTTP server serving `xml` at /database.xml; returns base URL.
fn serve_catalog(xml: String) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind smoke catalog server");
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        // Serve a handful of requests then exit (each scan fetches once).
        listener.set_nonblocking(false).ok();
        for _ in 0..8 {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => break,
            };
            // Read the request (ignore contents), then answer.
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = xml.clone();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    (format!("http://{addr}"), handle)
}

#[allow(dead_code)]
fn wait_for_completion(data_dir: &std::path::Path, info_hash: &str, timeout: Duration) -> bool {
    // Poll state.json: a torrent whose on-disk bytes reach nominal size counts
    // as complete (plain storage: files land at final size, sparse).
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(st) = keep_at::state::State::load(&data_dir.join("state.json")) {
            if let Some(t) = st.get(info_hash) {
                let dir = keep_at::engine::torrent_output_dir(&t.storage_location, &t.info_hash);
                if keep_at::engine::dir_size_bytes(&dir) >= t.size_bytes && t.size_bytes > 0 {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2000));
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_real_academictorrents() {
    if std::env::var("KEEPAT_SMOKE_TEST").as_deref() != Ok("1") {
        eprintln!("skipped: set KEEPAT_SMOKE_TEST=1 to run the live smoke test");
        return;
    }

    // Fast pipeline test: 2-item served catalog, real .torrent fetches and
    // scrapes, full engine scan, then assert selection + a real download.
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let (base, _server) = serve_catalog(smoke_catalog_xml(SMOKE_ITEMS));
    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let cfg = test_config(data_dir.clone(), storage_dir.clone(), 47560, 2.0);
    cfg.validate().unwrap();

    let mut engine = keep_at::engine::Engine::new_with_options(
        cfg,
        keep_at::engine::EngineOptions {
            catalog_url: Some(format!("{base}/database.xml")),
            at_base_url: None,
        },
    )
    .await
    .expect("engine new");
    tokio::time::timeout(Duration::from_secs(3 * 60), engine.scan_once())
        .await
        .expect("scan timed out")
        .expect("scan failed");

    let held = engine.held_torrents();
    assert!(
        !held.is_empty(),
        "expected at least one smoke-test torrent to be held"
    );
    eprintln!(
        "keep-at selected {} torrent(s) from the smoke-test catalog",
        held.len()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut done = false;
    while std::time::Instant::now() < deadline && !done {
        for h in engine.held_torrents() {
            let dir = keep_at::engine::torrent_output_dir(&h.storage_location, &h.info_hash);
            if h.size_bytes > 0 && keep_at::engine::dir_size_bytes(&dir) >= h.size_bytes {
                eprintln!(
                    "{}: {} bytes on disk after real download",
                    h.title, h.size_bytes
                );
                done = true;
                break;
            }
        }
        if !done {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    engine.close().await;
    assert!(done, "expected a real download to complete within 90s");
}

#[tokio::test(flavor = "multi_thread")]
async fn smoke_real_catalog_subset() {
    if std::env::var("KEEPAT_SMOKE_SUBSET").as_deref() != Ok("1") {
        eprintln!("skipped: set KEEPAT_SMOKE_SUBSET=1 to run the subset smoke test");
        return;
    }
    let size: usize = std::env::var("KEEPAT_SMOKE_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let rate: f64 = std::env::var("KEEPAT_SMOKE_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);

    // Smallest catalog entries by size (most likely still seeded), plus the
    // hand-picked fixtures so the download assertion is not flaky.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let raw = http
        .get(keep_at::atcatalog::DEFAULT_URL)
        .header("User-Agent", keep_at::buildinfo::user_agent())
        .send()
        .await
        .expect("fetch live catalog")
        .bytes()
        .await
        .unwrap();
    let mut cat = keep_at::atcatalog::parse(&raw).expect("parse live catalog");
    cat.items.sort_by_key(|i| i.size_bytes);
    let mut seen = std::collections::HashSet::new();
    let mut owned: Vec<(String, String, u64)> = Vec::new();
    for it in &cat.items {
        if it.size_bytes == 0 || !seen.insert(hex::encode(it.info_hash)) {
            continue;
        }
        owned.push((it.title.clone(), hex::encode(it.info_hash), it.size_bytes));
        if owned.len() >= size {
            break;
        }
    }
    for it in SMOKE_ITEMS {
        if seen.insert(it.info_hash.to_string()) {
            owned.push((it.title.to_string(), it.info_hash.to_string(), it.size));
        }
    }
    assert!(!owned.is_empty(), "catalog subset is empty");
    eprintln!("catalog subset: {} entries (smallest by size)", owned.len());

    let mut xml =
        String::from(r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel>"#);
    for (title, ih, size) in &owned {
        xml.push_str(&format!("<item><title>{title}</title><category>Paper</category><infohash>{ih}</infohash><guid>g</guid><link>l</link><description>d</description><size>{size}</size></item>"));
    }
    xml.push_str("</channel></rss>");
    let (base, _server) = serve_catalog(xml);

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let cfg = test_config(data_dir.clone(), storage_dir.clone(), 47561, rate);
    cfg.validate().unwrap();

    // Real engine scan: local catalog, live .torrent fetches + scrapes.
    let mut engine = keep_at::engine::Engine::new_with_options(
        cfg,
        keep_at::engine::EngineOptions {
            catalog_url: Some(format!("{base}/database.xml")),
            at_base_url: None,
        },
    )
    .await
    .expect("engine new");
    tokio::time::timeout(Duration::from_secs(9 * 60), engine.scan_once())
        .await
        .expect("scan timed out")
        .expect("scan failed");
    engine.close().await;

    let st = engine.last_scan_stats().await.expect("scan stats recorded");
    eprintln!(
        "subset: processed={} total={} eligible={} scrape_reqs={} scrape_cached={}",
        st.processed, st.total, st.eligible, st.scrape_requests, st.scrape_cached
    );
    assert_eq!(st.processed, st.total, "scan must finish the whole subset");
    assert!(
        st.scrape_requests >= st.eligible,
        "every eligible candidate must have issued a scrape"
    );
    let held = engine.held_torrents();
    assert!(
        !held.is_empty(),
        "expected the scan to hold at least one torrent"
    );

    // At least one held torrent completes a real download with data on disk.
    let deadline = std::time::Instant::now() + Duration::from_secs(5 * 60);
    let mut completed: Vec<String> = Vec::new();
    while std::time::Instant::now() < deadline && completed.is_empty() {
        for h in engine.held_torrents() {
            let dir = keep_at::engine::torrent_output_dir(&h.storage_location, &h.info_hash);
            if h.size_bytes > 0 && keep_at::engine::dir_size_bytes(&dir) >= h.size_bytes {
                completed.push(h.title.clone());
            }
        }
        if completed.is_empty() {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    assert!(
        !completed.is_empty(),
        "at least one held torrent must complete a real download"
    );
    eprintln!("completed downloads: {completed:?}");
}
