//! Maintenance paths: stall eviction (zero seeders + no progress past
//! timeout) and deleted-torrent removal (vanished from catalog), plus the
//! `preserve_deleted_torrents` opt-out. Both are destructive — the paths
//! that most need a regression net — and both run hermetic in one scan.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub, StubState};

fn setup(fixtures: &[Fixture]) -> (Stub, String, Vec<String>) {
    let (stub, catalog_base, hexes, _) = common::setup_with_catalog(fixtures);
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

    // History: the stall eviction is a Remove event with its reason.
    let (events, skipped) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
    assert_eq!(skipped, 0);
    let drops: Vec<&keep_at::history::Event> = events
        .iter()
        .filter(|e| matches!(e, keep_at::history::Event::Remove { .. }))
        .collect();
    assert_eq!(drops.len(), 1, "exactly one drop recorded");
    match drops[0] {
        keep_at::history::Event::Remove {
            hash,
            cause,
            reason,
            ..
        } => {
            assert_eq!(hash, &hex);
            assert_eq!(cause, &keep_at::history::Cause::Stalled);
            assert!(reason.contains("zero seeders"), "reason: {reason}");
        }
        _ => unreachable!(),
    }
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

/// Vanished-from-catalog torrents are removed once the vanished-eviction
/// grace expires; preserved outright with the flag.
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
        // This test targets the delisting-removal MECHANISM; the catalog
        // collapse guard (default 30%) deliberately blocks a 0-item catalog
        // (its own regression lives in tests/catalog_collapse.rs), so
        // disable it here to exercise the removal path. vanished timeout 0
        // = no grace, the pre-grace next-scan removal this test pins.
        cfg2.catalog_collapse_percent = 0;
        cfg2.scan.vanished_eviction_timeout = Duration::ZERO;
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

        // History: a delisting removal is recorded only when it happens.
        let (events, _) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
        let drops: Vec<&keep_at::history::Event> = events
            .iter()
            .filter(|e| matches!(e, keep_at::history::Event::Remove { .. }))
            .collect();
        if preserve {
            assert!(drops.is_empty(), "no drop recorded when preserved");
        } else {
            assert_eq!(drops.len(), 1);
            match drops[0] {
                keep_at::history::Event::Remove { hash, cause, .. } => {
                    assert_eq!(hash, &hex);
                    assert_eq!(cause, &keep_at::history::Cause::DeletedFromCatalog);
                }
                _ => unreachable!(),
            }
        }
    }
}

/// The vanished-eviction grace: a torrent missing from the catalog is held
/// (not removed) while absent for less than the timeout, even across scans,
/// and the confirmation stamp resets when the catalog lists it again.
#[tokio::test(flavor = "multi_thread")]
async fn vanished_grace_defers_removal() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let fx = Fixture::new("grace-survivor", 200_000, 1);
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
        test_port(49),
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

    // Phase 2: torrent vanishes. Grace is far in the future, so the removal
    // must be deferred (files and state survive) - and the confirmation
    // stamp must NOT advance (torrent is not listed anymore).
    let (cat2, _s2) = common::serve_catalog(Stub::catalog_xml(&[]));
    std::mem::forget(_s2);
    let mut cfg2 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(50),
        1 << 30,
    );
    cfg2.catalog_collapse_percent = 0;
    cfg2.scan.vanished_eviction_timeout = Duration::from_secs(90 * 24 * 3600);
    let mut engine2 = with_timeout(
        60,
        "engine2 new",
        keep_at::engine::Engine::new_with_options(cfg2, test_options(&cat2, &stub.base_url)),
    )
    .await
    .expect("engine2 new");
    with_timeout(180, "scan 2 (vanished, in grace)", engine2.scan_once())
        .await
        .expect("scan 2");
    engine2.close().await;

    assert_eq!(
        engine2.held_torrents().len(),
        1,
        "torrent within the vanished grace survives"
    );
    assert!(
        storage_dir.join(&hex).exists(),
        "grace survivor's data intact"
    );
    let state_path = data_dir.join("state.json");
    let raw = std::fs::read_to_string(&state_path).unwrap();
    let stamp_after_vanish = {
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v["torrents"][&hex]["last_confirmed_in_catalog_at"].clone()
    };

    // Phase 3: catalog hiccup ends - the torrent is listed again. The
    // confirmation stamp must reset to now, keeping the grace window full.
    let (cat3, _s3) = common::serve_catalog(Stub::catalog_xml(&[(
        fx.title.clone(),
        hex.clone(),
        fx.size,
    )]));
    std::mem::forget(_s3);
    let mut cfg3 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(51),
        1 << 30,
    );
    cfg3.scan.vanished_eviction_timeout = Duration::from_secs(90 * 24 * 3600);
    let mut engine3 = with_timeout(
        60,
        "engine3 new",
        keep_at::engine::Engine::new_with_options(cfg3, test_options(&cat3, &stub.base_url)),
    )
    .await
    .expect("engine3 new");
    with_timeout(180, "scan 3 (relisted)", engine3.scan_once())
        .await
        .expect("scan 3");
    engine3.close().await;

    assert_eq!(
        engine3.held_torrents().len(),
        1,
        "relisted torrent survives"
    );
    let raw3 = std::fs::read_to_string(&state_path).unwrap();
    let v3: serde_json::Value = serde_json::from_str(&raw3).unwrap();
    assert_ne!(
        v3["torrents"][&hex]["last_confirmed_in_catalog_at"], stamp_after_vanish,
        "confirmation stamp reset when the catalog lists the torrent again"
    );
}

/// The relaxed stall rule: a torrent that is INCOMPLETE but has live
/// seeders is evictable after the stall timeout (previously the seeders>0
/// exemption shielded it forever — that is how a broken-piece loop sat
/// hidden for weeks; see notes/DESIGN-broken-piece-quarantine.md). Fixture
/// torrents are permanently incomplete (their hash table is zeros), which
/// is exactly the scenario.
#[tokio::test(flavor = "multi_thread")]
async fn stall_eviction_incomplete_with_seeders_evicted() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let fx = Fixture::new("stuck-with-seeders", 200_000, 1);
    let (stub, _cat, hexes) = setup(std::slice::from_ref(&fx));
    let hex = hexes[0].clone();

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(61),
        1 << 30,
    );
    cfg.scan.stall_eviction_timeout = Duration::from_secs(3600); // survives phase 1
    let mut engine = with_timeout(
        60,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&_cat, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(180, "scan 1", engine.scan_once())
        .await
        .expect("scan 1");
    assert_eq!(engine.held_torrents().len(), 1, "phase 1 holds it");
    let out_dir = storage_dir.join(&hex);
    assert!(out_dir.exists());
    engine.close().await;

    // Stale progress clock, live swarm (1 seeder): the old rule shielded
    // this forever; the relaxed rule evicts after the timeout.
    {
        let mut st =
            keep_at::state::State::load(&data_dir.join("state.json")).expect("state loads");
        st.update(&hex, |t| {
            t.last_known_seeders = 1;
            t.last_progress_at = Some(chrono::Utc::now() - chrono::Duration::try_hours(2).unwrap());
        })
        .expect("state update");
    }

    let mut cfg2 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(62),
        1 << 30,
    );
    cfg2.scan.stall_eviction_timeout = Duration::from_secs(1);
    let mut engine2 = with_timeout(
        60,
        "engine2 new",
        keep_at::engine::Engine::new_with_options(cfg2, test_options(&_cat, &stub.base_url)),
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
        "incomplete torrent with seeders evicted after stall timeout"
    );
    assert!(!out_dir.exists(), "evicted output dir removed");
    let (events, skipped) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
    assert_eq!(skipped, 0);
    let drops: Vec<&keep_at::history::Event> = events
        .iter()
        .filter(|e| matches!(e, keep_at::history::Event::Remove { .. }))
        .collect();
    match drops[0] {
        keep_at::history::Event::Remove { cause, reason, .. } => {
            assert_eq!(cause, &keep_at::history::Cause::Stalled);
            assert!(
                reason.contains("1 seeders, incomplete"),
                "relaxed-rule reason string: {reason}"
            );
        }
        _ => unreachable!(),
    }
}

/// The fully-present guard: a COMPLETE torrent with a live swarm must
/// never be stall-evicted, no matter how stale its progress clock looks.
/// Its clock legitimately froze at completion (nothing left to verify);
/// the pre-relaxation code protected it via the seeders>0 exemption, and
/// the relaxation must not regress that. Uses a real-hash torrent so the
/// integrity check actually passes and rqbit reports it complete.
#[tokio::test(flavor = "multi_thread")]
async fn stall_eviction_complete_torrent_survives() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let fx = Fixture::new("complete-and-healthy", 200_000, 1);
    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    // Deterministic content + matching piece hashes; the data file is
    // written below so the boot integrity check passes.
    let content: Vec<u8> = (0..fx.size).map(|i| (i % 251) as u8).collect();
    let raw = common::torrent_bytes_with_real_pieces(&fx, &stub.tracker_url(), &content);
    let hex = common::torrent_info_hash(&raw);
    {
        let mut st = stub.state.lock().unwrap();
        st.torrents.insert(hex.clone(), raw.clone());
        st.scrapes.insert(hex.clone(), (1, 0));
    }

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let (cat1, _s1) = common::serve_catalog(Stub::catalog_xml(&[(
        fx.title.clone(),
        hex.clone(),
        fx.size,
    )]));
    std::mem::forget(_s1);

    // Seed state.json directly: torrent already complete (completed_pieces
    // == size), progress clock frozen 2h ago, live swarm. The cached
    // .torrent is written too — resume needs it, and WITH it the boot
    // adds the torrent into the session and the integrity check passes,
    // so the survival assertion pins rqbit's finished-flag guard (the
    // padding-file protection); without the cache file the test would
    // exercise only the handle-less dir-size fallback.
    let out_dir = storage_dir.join(&hex);
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(out_dir.join("complete-and-healthy.bin"), &content).unwrap();
    let cache_dir = data_dir.join("torrent-cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(cache_dir.join(format!("{hex}.torrent")), &raw).unwrap();
    {
        let mut st =
            keep_at::state::State::load(&data_dir.join("state.json")).expect("state loads");
        st.put(keep_at::state::Torrent {
            info_hash: hex.clone(),
            title: fx.title.clone(),
            size_bytes: fx.size,
            storage_location: storage_dir.clone(),
            added_at: chrono::Utc::now() - chrono::Duration::try_hours(3).unwrap(),
            piece_count: 1,
            last_known_seeders: 1,
            completed_pieces: fx.size,
            last_progress_at: Some(chrono::Utc::now() - chrono::Duration::try_hours(2).unwrap()),
            last_confirmed_in_catalog_at: None,
        })
        .expect("state put");
    }

    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(63),
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
    with_timeout(180, "scan", engine.scan_once())
        .await
        .expect("scan");
    engine.close().await;

    assert_eq!(
        engine.held_torrents().len(),
        1,
        "complete torrent survives the stall timeout regardless of its frozen clock"
    );
    let (events, skipped) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
    assert_eq!(skipped, 0);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, keep_at::history::Event::Remove { .. })),
        "no removal recorded for a healthy complete torrent"
    );
}

/// The Initializing exemption: a torrent mid-integrity-check reads as
/// "incomplete" (progress = bytes the check has walked so far) even when
/// the data is all there, and boots run hundreds of these — without the
/// exemption a post-crash boot would evict healthy data en masse. A big
/// real-piece fixture keeps the check running (seconds) while the scan's
/// eviction pass executes (~sub-second); if the machine is fast enough
/// that the check finishes first, the finished guard skips instead and
/// the test still passes (pinning the sibling guard).
#[tokio::test(flavor = "multi_thread")]
async fn stall_eviction_spares_check_in_flight() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    // 256 MiB, one piece: debug-profile hashing of the check takes
    // seconds — comfortably longer than the scan's path to evictions.
    let fx = Fixture::new("check-in-flight", 256 * 1024 * 1024, 1);
    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let content: Vec<u8> = (0..fx.size).map(|i| (i % 251) as u8).collect();
    let raw = common::torrent_bytes_with_real_pieces(&fx, &stub.tracker_url(), &content);
    let hex = common::torrent_info_hash(&raw);
    {
        let mut st = stub.state.lock().unwrap();
        st.torrents.insert(hex.clone(), raw.clone());
        st.scrapes.insert(hex.clone(), (1, 0));
    }
    let (cat1, _s1) = common::serve_catalog(Stub::catalog_xml(&[(
        fx.title.clone(),
        hex.clone(),
        fx.size,
    )]));
    std::mem::forget(_s1);

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let out_dir = storage_dir.join(&hex);
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(out_dir.join("check-in-flight.bin"), &content).unwrap();
    let cache_dir = data_dir.join("torrent-cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(cache_dir.join(format!("{hex}.torrent")), &raw).unwrap();
    {
        let mut st =
            keep_at::state::State::load(&data_dir.join("state.json")).expect("state loads");
        st.put(keep_at::state::Torrent {
            info_hash: hex.clone(),
            title: fx.title.clone(),
            size_bytes: fx.size,
            storage_location: storage_dir.clone(),
            added_at: chrono::Utc::now() - chrono::Duration::try_hours(3).unwrap(),
            piece_count: 1,
            last_known_seeders: 1,
            // Complete stamp: the mid-check partial progress must never
            // restamp the clock below it, keeping the eviction judgment
            // on the seeded 2h-stale stamp.
            completed_pieces: fx.size,
            last_progress_at: Some(chrono::Utc::now() - chrono::Duration::try_hours(2).unwrap()),
            last_confirmed_in_catalog_at: None,
        })
        .expect("state put");
    }

    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(64),
        1 << 30,
    );
    cfg.scan.stall_eviction_timeout = Duration::from_secs(1);
    let mut engine = with_timeout(
        120,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&cat1, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(300, "scan", engine.scan_once())
        .await
        .expect("scan");
    engine.close().await;

    assert_eq!(
        engine.held_torrents().len(),
        1,
        "torrent mid-integrity-check (or freshly finished) must not be stall-evicted"
    );
    let (events, skipped) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
    assert_eq!(skipped, 0);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, keep_at::history::Event::Remove { .. })),
        "no removal recorded for a check in flight"
    );
}

/// The handle-less eviction direction: a held torrent the session lost
/// (boot resume skipped it — no cached .torrent) with missing data
/// (dir absent → dir-size fallback reads 0) is evictable after the stall
/// timeout, and remove_torrent on an absent session handle must not fail
/// the scan. This is the "data dir deleted externally" recovery path.
#[tokio::test(flavor = "multi_thread")]
async fn stall_eviction_handleless_evicts_missing_data() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let fx = Fixture::new("handleless-gone", 200_000, 1);
    let (stub, _cat, hexes) = setup(std::slice::from_ref(&fx));
    let hex = hexes[0].clone();

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    // State entry ONLY: no cache file (resume skips → no session handle),
    // no output dir (data gone). Stale progress clock.
    {
        let mut st =
            keep_at::state::State::load(&data_dir.join("state.json")).expect("state loads");
        st.put(keep_at::state::Torrent {
            info_hash: hex.clone(),
            title: fx.title.clone(),
            size_bytes: fx.size,
            storage_location: storage_dir.clone(),
            added_at: chrono::Utc::now() - chrono::Duration::try_hours(3).unwrap(),
            piece_count: 0,
            last_known_seeders: 1,
            completed_pieces: 0,
            last_progress_at: Some(chrono::Utc::now() - chrono::Duration::try_hours(2).unwrap()),
            last_confirmed_in_catalog_at: None,
        })
        .expect("state put");
    }

    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(65),
        1 << 30,
    );
    cfg.scan.stall_eviction_timeout = Duration::from_secs(1);
    let mut engine = with_timeout(
        60,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&_cat, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(180, "scan", engine.scan_once())
        .await
        .expect("scan");
    engine.close().await;

    let held = engine.held_torrents();
    assert!(
        held.iter().all(|t| t.info_hash != hex),
        "handle-less torrent with missing data evicted after the stall timeout"
    );
    let (events, skipped) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
    assert_eq!(skipped, 0);
    let drops: Vec<&keep_at::history::Event> = events
        .iter()
        .filter(|e| matches!(e, keep_at::history::Event::Remove { .. }))
        .collect();
    match drops[0] {
        keep_at::history::Event::Remove { cause, .. } => {
            assert_eq!(cause, &keep_at::history::Cause::Stalled);
        }
        _ => unreachable!(),
    }
}
