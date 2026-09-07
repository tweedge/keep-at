//! Tracker discipline: 429 backoff without cache poisoning, UDP-only
//! torrents skipped quietly with zero scrape requests, AT hosts
//! rate-limited while third-party trackers pass through untouched.
//!
//! The 429 backoff is injectable via Options (10ms in these tests), so the
//! backoff PATH runs fast instead of being skipped or slow.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub, StubState};

fn setup(fixtures: &[Fixture]) -> (Stub, String, Vec<String>, Arc<Mutex<StubState>>) {
    let state = Arc::new(Mutex::new(StubState::default()));
    let mut raws = Vec::new();
    for f in fixtures {
        raws.push((
            f.clone(),
            common::torrent_bytes(f, "http://127.0.0.1:9/announce"),
        ));
    }
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let tracker = stub.tracker_url();
    let mut rows = Vec::new();
    let mut hexes = Vec::new();
    {
        let mut st = state.lock().unwrap();
        for (f, _) in &raws {
            let (hex, _) = st.add(f, &tracker);
            rows.push((f.title.clone(), hex.clone(), f.size));
            hexes.push(hex);
        }
    }
    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&rows));
    std::mem::forget(_srv);
    (stub, catalog_base, hexes, state)
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_backs_off_without_poisoning_cache() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let fx = Fixture::new("throttled", 100_000, 4);
    let (stub, catalog_base, _hexes, state) = setup(std::slice::from_ref(&fx));

    // The FIRST scrape request 429s; the rest succeed. The fixture has ONE
    // tracker, so a failing scrape backs off and fails the candidate for
    // THIS scan (fail-fast protects the shared budget) — recovery happens
    // on the NEXT scan, which is exactly the production contract ("nothing
    // is cached; the next scan retries these"). Assert scan 1 skips without
    // poisoning, scan 2 recovers and holds.
    state.lock().unwrap().fail_scrapes_429 = 1;

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(61),
        1 << 30,
    );
    let mut engine = with_timeout(
        60,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url)),
    )
    .await
    .expect("engine new");
    // Backoff is 10ms in tests: scan 1 fails fast on the 429s (fail-fast
    // protects the shared budget), scan 2 recovers and holds. A 60s
    // production backoff would make even one scan take minutes — the
    // injectable Options field exists so this PATH runs fast.
    with_timeout(120, "scan 1", engine.scan_once())
        .await
        .expect("scan 1");
    let stats1 = engine.last_scan_stats().await.expect("scan 1 stats");
    assert_eq!(
        stats1.eligible, 0,
        "429s fail the candidate fast, nothing cached"
    );
    assert!(engine.held_torrents().is_empty(), "nothing held after 429s");
    engine.close().await;

    // Scan 2 (same engine? No — engine is closed; the cache lives in the
    // data dir. Fresh engine, same dirs: scrape-cache has no entry (failures
    // never cache), stub 429 budget spent, success path serves counts.
    let cfg2 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(66),
        1 << 30,
    );
    let mut engine2 = with_timeout(
        60,
        "engine2 new",
        keep_at::engine::Engine::new_with_options(
            cfg2,
            test_options(&catalog_base, &stub.base_url),
        ),
    )
    .await
    .expect("engine2 new");
    with_timeout(120, "scan 2", engine2.scan_once())
        .await
        .expect("scan 2");
    engine2.close().await;

    let stats = engine2.last_scan_stats().await.expect("scan 2 stats");
    // The candidate recovered after the 429s (not skipped, not cached-bad).
    assert_eq!(stats.eligible, 1, "candidate eligible after 429 recovery");
    assert_eq!(engine2.held_torrents().len(), 1, "recovered candidate held");
    // Nothing cached from the failure: scan 2's success wrote real counts.
    // (Proven by eligible=1 above — a poisoned cache would serve the 429 as
    // data or skip the scrape entirely.)
    let st = state.lock().unwrap();
    assert_eq!(st.scrape_requests, 2, "one failed scrape + one success");
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_only_torrent_skipped_quietly() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    // Fixture whose ONLY tracker is UDP: no HTTPS tracker to scrape.
    let fx = Fixture::new("udp-only", 100_000, 4);
    let state = Arc::new(Mutex::new(StubState::default()));
    let raw = common::torrent_bytes(&fx, "udp://127.0.0.1:6969/announce");
    let hex = common::torrent_info_hash(&raw);
    {
        let mut st = state.lock().unwrap();
        st.torrents.insert(hex.clone(), raw);
        // No scrape entry needed: nothing should ever ask.
    }
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&[(
        fx.title.clone(),
        hex.clone(),
        fx.size,
    )]));
    std::mem::forget(_srv);

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let cfg = test_config(data_dir.clone(), storage_dir, test_port(62), 1 << 30);
    let mut engine = with_timeout(
        60,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(120, "scan", engine.scan_once())
        .await
        .expect("scan");
    engine.close().await;

    let stats = engine.last_scan_stats().await.expect("scan stats");
    assert_eq!(stats.eligible, 0, "UDP-only torrent never eligible");
    assert_eq!(
        stats.scrape_requests, 0,
        "zero scrape requests issued for UDP-only torrent"
    );
    assert!(engine.held_torrents().is_empty(), "nothing held");
    let st = state.lock().unwrap();
    assert!(
        st.scrape_hits.is_empty(),
        "stub saw no scrape hits at all: {:?}",
        st.scrape_hits
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn at_hosts_rate_limited_third_party_untouched() {
    // Unit-level pin of the routing rule the engine relies on: only AT
    // tracker hosts go through the shared limiter. The engine calls
    // is_at_tracker_url per tracker before waiting; assert the predicate
    // classifies a matrix of URLs the way the scan path needs.
    assert!(keep_at::atkey::is_at_tracker_url(
        "https://academictorrents.com/announce.php"
    ));
    assert!(keep_at::atkey::is_at_tracker_url(
        "https://ipv6.academictorrents.com/announce.php?passkey=x"
    ));
    assert!(!keep_at::atkey::is_at_tracker_url(
        "https://tracker.opentrackr.org:1337/announce"
    ));
    assert!(!keep_at::atkey::is_at_tracker_url(
        "udp://tracker.openbittorrent.com:80/announce"
    ));
    // Keyed URLs keep their AT host (key never leaks classification).
    let keyed = keep_at::atkey::at_announce_url(
        "https://academictorrents.com/announce.php",
        "https://academictorrents.com/announce.php?passkey=K",
        "",
    )
    .expect("keyed");
    assert!(keep_at::atkey::is_at_tracker_url(&keyed));
    let _ = Duration::ZERO;
}
