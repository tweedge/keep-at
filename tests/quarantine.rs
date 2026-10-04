//! Broken-piece quarantine end-to-end: the only paths that ever RELEASE a
//! quarantine (lift branches in `quarantine_pass`), and the registry GC
//! that must NOT release anything still re-probe-able. An earlier review
//! found both lift branches had zero coverage; a "never lifts" regression
//! permanently locks a fixed torrent out (manual state.json surgery), a
//! "lifts wrongly" regression re-admits a still-broken swarm (the
//! original ~55 GiB/day incident shape), and a GC that drops entries for
//! still-listed hashes silently cancels every documented re-probe (plus
//! resets the attempts count that `max_retries` escalation rides on).
//! All three failure modes are silent in production, so these tests pin
//! them.
//!
//! Test 1 (sweep finished-branch lift): a held torrent whose re-probe
//! COMPLETED with a registry entry → entry lifted, data intact, no
//! removal recorded.
//! Strictness pin: a held-but-incomplete torrent (zero piece hashes — the
//! fixture stand-in for a mixed/poisoned swarm) with a registry entry →
//! the entry SURVIVES the pass. Only completion lifts.
//!
//! Test 2 (registry GC truth table): an orphaned entry is dropped ONLY
//! when its hash is lapsed + not held + not in session + NOT in the last
//! fetched catalog. A still-listed lapsed hash keeps its entry (and its
//! attempts count) until its probe runs; an active-cooldown orphan stays
//! for the relist window.
//!
//! ~75-90s wall time per test (the watchdog interval floors at 60s; the
//! first pass fires there). Deliberate: no cheaper harness exercises the
//! pass.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub, StubState};

static DHT_STATE: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

fn seed_completed_scan(data_dir: &std::path::Path, completed: chrono::DateTime<chrono::Utc>) {
    keep_at::netstats::save_snapshot(
        &data_dir.join("network-stats.json"),
        &keep_at::netstats::Snapshot {
            scan_started_at: Some(completed - chrono::Duration::try_hours(1).unwrap()),
            scan_completed_at: Some(completed),
            total_candidates: 0,
            processed_candidates: 0,
            seeder_floor: 4,
        },
    )
    .unwrap();
}

fn quarantine_entry(attempts: u32) -> keep_at::state::Quarantine {
    keep_at::state::Quarantine {
        title: "t".to_string(),
        reason: "discarded 300 MiB across 2 zero-progress passes (attempt 1)".to_string(),
        quarantined_at: chrono::Utc::now(),
        cooldown_until: chrono::Utc::now() + chrono::Duration::try_hours(1).unwrap(),
        attempts,
        wasted_bytes: 300 * 1024 * 1024,
    }
}

/// Seed data_dir so ONE torrent boots as held-and-complete: cached
/// .torrent (resume needs it), files on disk, state entry complete, and
/// a quarantine registry entry waiting to be lifted by completion.
fn seed_completed_held(
    data_dir: &std::path::Path,
    storage_dir: &std::path::Path,
    fx: &Fixture,
    raw: &[u8],
    hex: &str,
    attempts: u32,
) {
    let cache_dir = data_dir.join("torrent-cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(cache_dir.join(format!("{hex}.torrent")), raw).unwrap();
    let out_dir = storage_dir.join(hex);
    std::fs::create_dir_all(&out_dir).unwrap();
    let content: Vec<u8> = (0..fx.size).map(|i| (i % 251) as u8).collect();
    std::fs::write(out_dir.join(format!("{}.bin", fx.title)), &content).unwrap();
    let mut st = keep_at::state::State::load(&data_dir.join("state.json")).expect("state loads");
    st.put(keep_at::state::Torrent {
        info_hash: hex.to_string(),
        title: fx.title.clone(),
        size_bytes: fx.size,
        storage_location: storage_dir.to_path_buf(),
        added_at: chrono::Utc::now() - chrono::Duration::try_hours(3).unwrap(),
        piece_count: 1,
        last_known_seeders: 1,
        completed_pieces: fx.size,
        last_progress_at: Some(chrono::Utc::now() - chrono::Duration::try_hours(2).unwrap()),
        last_confirmed_in_catalog_at: None,
    })
    .unwrap();
    st.quarantine_put(hex.to_string(), quarantine_entry(attempts))
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn quarantine_lifts_when_reprobe_completes_and_spares_incomplete() {
    tokio::task::LocalSet::new().run_until(lifts_body()).await;
}

async fn lifts_body() {
    let _dht_guard = DHT_STATE.lock().await;
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    // Torrent A: real piece hashes + matching files → boot check passes →
    // finished → the sweep's finished-branch lift fires.
    let fxa = Fixture::new("reprobe-completes", 200_000, 1);
    // Torrent B: the standard zero-hash fixture → the boot check can
    // NEVER pass (fixture stand-in for a mixed/poisoned swarm) → entry
    // must survive.
    let fxb = Fixture::new("reprobe-stuck", 200_000, 1);
    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());

    let raw_a = common::torrent_bytes_with_real_pieces(&fxa, &stub.tracker_url(), &{
        (0..fxa.size).map(|i| (i % 251) as u8).collect::<Vec<u8>>()
    });
    let hex_a = common::torrent_info_hash(&raw_a);
    let raw_b = common::torrent_bytes(&fxb, &stub.tracker_url());
    let hex_b = common::torrent_info_hash(&raw_b);
    {
        let mut st = stub.state.lock().unwrap();
        st.torrents.insert(hex_a.clone(), raw_a.clone());
        st.torrents.insert(hex_b.clone(), raw_b.clone());
        st.scrapes.insert(hex_a.clone(), (1, 0));
        st.scrapes.insert(hex_b.clone(), (1, 0));
    }
    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&[
        (fxa.title.clone(), hex_a.clone(), fxa.size),
        (fxb.title.clone(), hex_b.clone(), fxb.size),
    ]));
    std::mem::forget(_srv);

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    seed_completed_held(&data_dir, &storage_dir, &fxa, &raw_a, &hex_a, 1);
    seed_completed_held(&data_dir, &storage_dir, &fxb, &raw_b, &hex_b, 1);
    seed_completed_scan(&data_dir, chrono::Utc::now());

    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(93),
        1 << 30,
    );
    // No scans during the test (3600s cadence, stamp seeded "now"); the
    // watchdog runs at its 60s floor; stall eviction off (both torrents
    // carry stale clocks by construction).
    cfg.scan.interval = Duration::from_secs(3600);
    cfg.scan.quarantine_check_interval = Duration::from_secs(60);
    cfg.scan.stall_eviction_timeout = Duration::ZERO;

    let mut engine =
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url))
            .await
            .expect("engine new");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let run_task = tokio::task::spawn_local(async move { engine.run(shutdown_rx).await });

    // Poll for the lift: entry A gone, entry B retained.
    let poll_deadline = Instant::now() + Duration::from_secs(110);
    let count_b = loop {
        let st = keep_at::state::State::load(&data_dir.join("state.json")).unwrap();
        let lifted = st.quarantine_get(&hex_a).is_none();
        let count_b = st.quarantine_get(&hex_b).is_some();
        if lifted {
            break count_b;
        }
        assert!(
            Instant::now() < poll_deadline,
            "completed re-probe did not lift within the watchdog window"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert!(
        count_b,
        "incomplete (poisoned-stand-in) re-probe must NOT lift — only completion lifts"
    );

    // Lift must not damage the torrent: still held, files intact, no
    // removal recorded for either hash.
    let held: Vec<String> = common::read_held(&data_dir)
        .iter()
        .map(|t| t.info_hash.clone())
        .collect();
    assert!(held.contains(&hex_a), "lifted torrent stays held");
    assert!(held.contains(&hex_b), "stuck torrent stays held");
    assert!(
        storage_dir
            .join(&hex_a)
            .join(format!("{}.bin", fxa.title))
            .exists(),
        "lift must not delete data"
    );
    let (events, skipped) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
    assert_eq!(skipped, 0);
    assert!(
        !events.iter().any(
            |e| matches!(e, keep_at::history::Event::Remove { cause, .. }
                if *cause == keep_at::history::Cause::Quarantined)
        ),
        "a lift is not a removal"
    );

    shutdown_tx.send(true).unwrap();
    with_timeout(30, "run shutdown", run_task)
        .await
        .expect("run task joins")
        .expect("run returns Ok");
}

/// Registry GC truth table. An orphaned entry (not held, not in session)
/// is garbage-collected ONLY when it is ALSO lapsed AND absent from the
/// last fetched catalog:
/// - delisted + lapsed → GC'd (the accumulate-forever case);
/// - listed + lapsed → KEPT with its attempts count: the documented
///   re-probe happens at the next scan, and a GC that fires first (the
///   watchdog passes every 30m, scans run every few days) would silently
///   cancel it and reset `max_retries` escalation;
/// - delisted + active cooldown → KEPT for the relist window.
#[tokio::test(flavor = "current_thread")]
async fn orphan_gc_requires_unlisted_and_lapsed() {
    tokio::task::LocalSet::new()
        .run_until(orphan_gc_body())
        .await;
}

async fn orphan_gc_body() {
    let _dht_guard = DHT_STATE.lock().await;
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let hex_delisted_lapsed = "aa".repeat(20);
    let hex_listed_lapsed = "bb".repeat(20);
    let hex_delisted_active = "cc".repeat(20);

    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    // Only B is catalog-listed; the scan will try to probe it and fail
    // fast at the stub (no torrent bytes) — the entry must survive that.
    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&[(
        "listed-orphan".to_string(),
        hex_listed_lapsed.clone(),
        50_000,
    )]));
    std::mem::forget(_srv);

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    {
        let mut st =
            keep_at::state::State::load(&data_dir.join("state.json")).expect("state loads");
        let lapsed = chrono::Utc::now() - chrono::Duration::try_hours(1).unwrap();
        let entry = |title: &str, cooldown_until: chrono::DateTime<chrono::Utc>, attempts: u32| {
            keep_at::state::Quarantine {
                title: title.to_string(),
                reason: "discarded 300 MiB across 2 zero-progress passes (attempt 1)".to_string(),
                quarantined_at: lapsed,
                cooldown_until,
                attempts,
                wasted_bytes: 300 * 1024 * 1024,
            }
        };
        st.quarantine_put(
            hex_delisted_lapsed.clone(),
            entry("delisted-orphan", lapsed, 2),
        )
        .unwrap();
        st.quarantine_put(hex_listed_lapsed.clone(), entry("listed-orphan", lapsed, 3))
            .unwrap();
        st.quarantine_put(
            hex_delisted_active.clone(),
            entry(
                "active-orphan",
                chrono::Utc::now() + chrono::Duration::try_hours(1).unwrap(),
                4,
            ),
        )
        .unwrap();
    }

    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(94),
        1 << 30,
    );
    // One scan only (the boot scan populates the catalog set the GC
    // consults); the watchdog then passes every 60s. No completed-scan
    // stamp seeded → the boot scan runs immediately.
    cfg.scan.interval = Duration::from_secs(3600);
    cfg.scan.quarantine_check_interval = Duration::from_secs(60);
    cfg.scan.stall_eviction_timeout = Duration::ZERO;

    let mut engine =
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url))
            .await
            .expect("engine new");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let run_task = tokio::task::spawn_local(async move { engine.run(shutdown_rx).await });

    // Poll past the first watchdog pass (fires at +60s).
    let poll_deadline = Instant::now() + Duration::from_secs(110);
    loop {
        let st = keep_at::state::State::load(&data_dir.join("state.json")).unwrap();
        if st.quarantine_get(&hex_delisted_lapsed).is_none() {
            break;
        }
        assert!(
            Instant::now() < poll_deadline,
            "delisted+lapsed orphan was not GC'd within the watchdog window"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let st = keep_at::state::State::load(&data_dir.join("state.json")).unwrap();
    let listed = st
        .quarantine_get(&hex_listed_lapsed)
        .expect("listed+lapsed entry must survive GC — its re-probe is still pending");
    assert_eq!(
        listed.attempts, 3,
        "attempts count preserved for escalation"
    );
    let active = st
        .quarantine_get(&hex_delisted_active)
        .expect("active-cooldown orphan must survive GC (relist window)");
    assert_eq!(active.attempts, 4);
    assert!(
        common::read_held(&data_dir).is_empty(),
        "the unfetchable listed hash's failed probe holds nothing"
    );

    shutdown_tx.send(true).unwrap();
    with_timeout(30, "run shutdown", run_task)
        .await
        .expect("run task joins")
        .expect("run returns Ok");
}
