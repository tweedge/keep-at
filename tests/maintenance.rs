//! Maintenance paths: stall eviction (zero seeders + no progress past
//! timeout) and deleted-torrent removal (vanished from catalog), plus the
//! `preserve_deleted_torrents` opt-out. Both are destructive — the paths
//! that most need a regression net — and both run hermetic in one scan.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub, StubState};

fn setup(fixtures: &[Fixture]) -> (Stub, String, Vec<String>) {
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
    (stub, catalog_base, hexes)
}

/// Hold one torrent, then time-travel its progress clock past the stall
/// timeout with zero seeders: scan 2 must evict it (files gone, state gone).
#[tokio::test(flavor = "multi_thread")]
async fn stall_eviction_removes_quiet_zero_seeder() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    // Phase 1: hold a 1-seeder torrent (gate open, eligible).
    let fx = Fixture::new("doomed", 200_000, 1);
    let (stub, _cat, hexes) = setup(std::slice::from_ref(&fx));
    let hex = hexes[0].clone();

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let (cat1, _s1) = common::serve_catalog(Stub::catalog_xml(&[(
        fx.title.clone(),
        hex.clone(),
        fx.size,
    )]));
    std::mem::forget(_s1);
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(41),
        1 << 30,
    );
    // Stall timeout 1s so the test doesn't wait two weeks.
    cfg.scan.stall_eviction_timeout = Duration::from_secs(1);
    let mut engine = with_timeout(
        60,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&cat1, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(180, "scan 1", engine.scan_once())
        .await
        .expect("scan 1");
    assert_eq!(engine.held_torrents().len(), 1, "phase 1 holds it");
    let out_dir = storage_dir.join(&hex);
    assert!(out_dir.exists(), "output dir exists");
    engine.close().await;

    // Time-travel: last progress 1h ago, zero seeders (stub now says 0).
    {
        let mut st = stub.state.lock().unwrap();
        st.scrapes.insert(hex.clone(), (0, 0));
    }
    {
        let mut st =
            keep_at::state::State::load(&data_dir.join("state.json")).expect("state loads");
        st.update(&hex, |t| {
            t.last_known_seeders = 0;
            t.last_progress_at = Some(chrono::Utc::now() - chrono::Duration::try_hours(1).unwrap());
        })
        .expect("state update");
    }

    // Phase 2: same catalog. Held refresh scrapes 0 seeders, progress
    // unchanged for 1h > 1s timeout -> evicted.
    let mut cfg2 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(42),
        1 << 30,
    );
    cfg2.scan.stall_eviction_timeout = Duration::from_secs(1);
    let mut engine2 = with_timeout(
        60,
        "engine2 new",
        keep_at::engine::Engine::new_with_options(cfg2, test_options(&cat1, &stub.base_url)),
    )
    .await
    .expect("engine2 new");
    with_timeout(180, "scan 2", engine2.scan_once())
        .await
        .expect("scan 2");
    engine2.close().await;

    let held2 = engine2.held_torrents();
    assert!(
        held2.iter().all(|t| t.info_hash != hex),
        "stalled torrent evicted"
    );
    assert!(!out_dir.exists(), "evicted output dir removed");
}

/// A live (recent-progress) zero-seeder is NOT evicted.
#[tokio::test(flavor = "multi_thread")]
async fn live_zero_seeder_survives() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let fx = Fixture::new("patient", 200_000, 1);
    let (stub, _cat, hexes) = setup(std::slice::from_ref(&fx));
    let hex = hexes[0].clone();

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let (cat1, _s1) = common::serve_catalog(Stub::catalog_xml(&[(
        fx.title.clone(),
        hex.clone(),
        fx.size,
    )]));
    std::mem::forget(_s1);
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(43),
        1 << 30,
    );
    cfg.scan.stall_eviction_timeout = Duration::from_secs(1);
    let mut engine = with_timeout(
        60,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&cat1, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(180, "scan 1", engine.scan_once())
        .await
        .expect("scan 1");
    assert_eq!(engine.held_torrents().len(), 1);
    engine.close().await;

    // Zero seeders now, but progress clock is FRESH (just added) and the
    // torrent is still downloading (progress_bytes grows or dir exists).
    {
        let mut st = stub.state.lock().unwrap();
        st.scrapes.insert(hex.clone(), (0, 0));
    }
    // Touch the output dir so on-disk proxy shows presence; the timeout is
    // 1s but last_progress_at is now (scan-1 add time is seconds ago...
    // to be safe, bump it to now explicitly).
    {
        let mut st =
            keep_at::state::State::load(&data_dir.join("state.json")).expect("state loads");
        st.update(&hex, |t| {
            t.last_known_seeders = 0;
            t.last_progress_at = Some(chrono::Utc::now());
        })
        .expect("state update");
    }
    // Stall timeout LONGER than the clock age: 1h. Fresh progress survives.
    let mut cfg2 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(44),
        1 << 30,
    );
    cfg2.scan.stall_eviction_timeout = Duration::from_secs(3600);
    let mut engine2 = with_timeout(
        60,
        "engine2 new",
        keep_at::engine::Engine::new_with_options(cfg2, test_options(&cat1, &stub.base_url)),
    )
    .await
    .expect("engine2 new");
    // Sleep past... no: timeout 3600s, clock age ~seconds. Scan immediately.
    with_timeout(180, "scan 2", engine2.scan_once())
        .await
        .expect("scan 2");
    engine2.close().await;

    assert!(
        engine2.held_torrents().iter().any(|t| t.info_hash == hex),
        "fresh zero-seeder survives"
    );
}

/// Vanished-from-catalog torrents are removed; preserved with the flag.
#[tokio::test(flavor = "multi_thread")]
async fn deleted_torrents_removed_unless_preserved() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    for preserve in [false, true] {
        let fx = Fixture::new("listed-then-gone", 200_000, 1);
        let (stub, _cat, hexes) = setup(std::slice::from_ref(&fx));
        let hex = hexes[0].clone();

        let data_dir = tempfile::tempdir().unwrap().keep();
        let storage_dir = tempfile::tempdir().unwrap().keep();
        let (cat1, _s1) = common::serve_catalog(Stub::catalog_xml(&[(
            fx.title.clone(),
            hex.clone(),
            fx.size,
        )]));
        std::mem::forget(_s1);
        let cfg = test_config(
            data_dir.clone(),
            storage_dir.clone(),
            test_port(if preserve { 45 } else { 46 }),
            1 << 30,
        );
        let mut engine = with_timeout(
            60,
            "engine new",
            keep_at::engine::Engine::new_with_options(cfg, test_options(&cat1, &stub.base_url)),
        )
        .await
        .expect("engine new");
        with_timeout(180, "scan 1", engine.scan_once())
            .await
            .expect("scan 1");
        assert_eq!(engine.held_torrents().len(), 1);
        engine.close().await;

        // Phase 2: EMPTY catalog. Held torrent vanished.
        let (cat2, _s2) = common::serve_catalog(Stub::catalog_xml(&[]));
        std::mem::forget(_s2);
        let mut cfg2 = test_config(
            data_dir.clone(),
            storage_dir.clone(),
            test_port(if preserve { 47 } else { 48 }),
            1 << 30,
        );
        cfg2.preserve_deleted_torrents = preserve;
        let mut engine2 = with_timeout(
            60,
            "engine2 new",
            keep_at::engine::Engine::new_with_options(cfg2, test_options(&cat2, &stub.base_url)),
        )
        .await
        .expect("engine2 new");
        with_timeout(180, "scan 2", engine2.scan_once())
            .await
            .expect("scan 2");
        engine2.close().await;

        let held2 = engine2.held_torrents();
        if preserve {
            assert!(
                held2.iter().any(|t| t.info_hash == hex),
                "preserved torrent survives delisting"
            );
        } else {
            assert!(
                held2.iter().all(|t| t.info_hash != hex),
                "delisted torrent removed"
            );
            assert!(!storage_dir.join(&hex).exists(), "removed output dir gone");
        }
    }
}
