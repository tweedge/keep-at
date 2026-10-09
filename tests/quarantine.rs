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

/// A registry entry whose cooldown has LAPSED — eligible for re-probe.
fn lapsed_quarantine_entry(title: &str, attempts: u32) -> keep_at::state::Quarantine {
    keep_at::state::Quarantine {
        title: title.to_string(),
        reason: format!("discarded 300 MiB across 2 zero-progress passes (attempt {attempts})"),
        quarantined_at: chrono::Utc::now() - chrono::Duration::try_hours(6).unwrap(),
        cooldown_until: chrono::Utc::now() - chrono::Duration::try_minutes(5).unwrap(),
        attempts,
        wasted_bytes: 300 * 1024 * 1024,
    }
}

/// The re-probe must run on the quarantine watchdog, not wait for a scan.
///
/// This is the regression the v0.8.31 deployment exposed: the cooldown
/// expiry is documented to BE the periodic re-probe, but the re-add lived
/// only inside the scan's candidate evaluation — so on a host with a 7-day
/// scan interval a 3-day cooldown meant "re-probe after up to 10 days".
/// Mercury sat in exactly that gap: cooldown lapsed Oct 7, next scan Oct 10.
///
/// No scan runs here (`scan.interval` is an hour away and the pass is
/// driven directly), so anything added is the watchdog's doing.
#[tokio::test(flavor = "multi_thread")]
async fn watchdog_reprobes_lapsed_entry_without_a_scan() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    // 2000 seeders: the availability veto (at least one live seed) passes,
    // but the seed-scarcity chance underflows to exactly 0.0 — so only the
    // probe bypass can admit it. This makes the test pin the bypass too, not
    // just the timer.
    let fx = Fixture::new("lapsed-reprobe", 200_000, 2000);
    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let raw = common::torrent_bytes(&fx, &stub.tracker_url());
    let hex = common::torrent_info_hash(&raw);
    {
        let mut st = state.lock().unwrap();
        st.torrents.insert(hex.clone(), raw.clone());
        st.scrapes.insert(hex.clone(), (2000, 0));
    }
    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&[(
        fx.title.clone(),
        hex.clone(),
        fx.size,
    )]));
    std::mem::forget(_srv);

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let cache_dir = data_dir.join("torrent-cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(cache_dir.join(format!("{hex}.torrent")), &raw).unwrap();
    {
        let mut st = keep_at::state::State::load(&data_dir.join("state.json")).expect("state");
        st.quarantine_put(hex.clone(), lapsed_quarantine_entry(&fx.title, 1))
            .expect("registry seed");
    }

    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(97),
        1 << 30,
    );
    cfg.aggressiveness = 0.5; // 0.5^(2000-1) underflows to 0.0
                              // A scan must not be able to explain the add: its next due is an hour
                              // out, and we drive the watchdog pass directly.
    cfg.scan.interval = Duration::from_secs(3600);
    cfg.scan.quarantine_check_interval = Duration::from_secs(60);
    // The sharp case: "no cooldown" is a legal config, and without a re-arm
    // floor it would re-probe a known-poisoned swarm on every watchdog tick.
    cfg.scan.quarantine_cooldown = Duration::ZERO;
    let mut engine =
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url))
            .await
            .expect("engine new");

    let (_sd_tx, sd_rx) = tokio::sync::watch::channel(false);
    engine.quarantine_pass(&sd_rx).await;

    let held: Vec<String> = engine
        .held_torrents()
        .into_iter()
        .map(|t| t.info_hash)
        .collect();
    assert!(
        held.contains(&hex),
        "the watchdog must re-probe a lapsed entry on its own timer, without a \
         scan — a cooldown that only re-probes at scan time means a 3-day \
         cooldown on a 7-day scan cadence waits up to 10 days. held: {held:?}"
    );

    // Probing is not a new attempt: attempts counts QUARANTINES (trips), and
    // the 3-day cooldown stays the clock that paces retry exhaustion. If
    // probing incremented attempts, probing sooner would exhaust max_retries
    // faster and lock out a recoverable torrent.
    let st = keep_at::state::State::load(&data_dir.join("state.json")).expect("state");
    let q = st
        .quarantine_get(&hex)
        .expect("entry survives its own probe");
    assert_eq!(q.attempts, 1, "a probe must NOT increment attempts");

    // Churn guard: probing re-arms the cooldown, so the next probe is a
    // cooldown away rather than on the next watchdog tick.
    assert!(
        q.cooldown_until > chrono::Utc::now(),
        "the probe must re-arm the cooldown (floored at the watchdog cadence, \
         so `quarantine_cooldown: 0` cannot churn) — otherwise a stuck probe \
         is re-added every watchdog tick (cooldown_until={})",
        q.cooldown_until
    );

    // Second pass must not double-add.
    let (_sd_tx, sd_rx) = tokio::sync::watch::channel(false);
    engine.quarantine_pass(&sd_rx).await;
    let held2: Vec<String> = engine
        .held_torrents()
        .into_iter()
        .map(|t| t.info_hash)
        .collect();
    assert_eq!(held2.len(), 1, "a second pass must not re-add: {held2:?}");
    engine.close().await;
}

/// The safety rule must hold on the new timer path exactly as it does on the
/// scan path: a probe is speculative and `try_swap` DELETES the displaced
/// torrent's data, so a probe must never displace a healthy held torrent.
///
/// The setup is the mirror of tests/scan_gate.rs::quarantine_probe_never_displaces_held,
/// which pins the same rule for the scan path — the victim fills the
/// location first (phase 1), then a lapsed probe arrives that only fits by
/// displacing it (phase 2).
#[tokio::test(flavor = "multi_thread")]
async fn watchdog_probe_never_displaces_held() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    let held_fx = Fixture::new("held-safe", 100_000_000, 10);
    let probe_fx = Fixture::new("probe-lapsed", 51_200_000, 5);
    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());

    let mut rows = Vec::new();
    let mut raws = Vec::new();
    for fx in [&held_fx, &probe_fx] {
        let raw = common::torrent_bytes(fx, &stub.tracker_url());
        let hex = common::torrent_info_hash(&raw);
        {
            let mut st = state.lock().unwrap();
            st.torrents.insert(hex.clone(), raw.clone());
            st.scrapes.insert(hex.clone(), (fx.seeders, 0));
        }
        rows.push((fx.title.clone(), hex.clone(), fx.size));
        raws.push((hex, raw));
    }
    let (victim_hex, probe_hex) = (raws[0].0.clone(), raws[1].0.clone());

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let cache_dir = data_dir.join("torrent-cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    for (hex, raw) in &raws {
        std::fs::write(cache_dir.join(format!("{hex}.torrent")), raw).unwrap();
    }

    // Phase 1: ONLY the victim is listed, so it fills the location alone
    // (100 MB into a 150 MB limit leaves ~50 MB free).
    let (cat1, _s1) = common::serve_catalog(Stub::catalog_xml(&[rows[0].clone()]));
    std::mem::forget(_s1);
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(98),
        150_000_000,
    );
    // Force the scarcity gate open: this test is about displacement, and the
    // victim must actually be held before there is anything to displace.
    cfg.aggressiveness = 0.999999;
    cfg.scan.interval = Duration::from_secs(3600);
    cfg.scan.quarantine_check_interval = Duration::from_secs(60);
    cfg.scan.min_seed_margin = 4;
    let mut engine =
        keep_at::engine::Engine::new_with_options(cfg, test_options(&cat1, &stub.base_url))
            .await
            .expect("engine new");
    engine.scan_once().await.expect("scan holds the victim");
    let out_dir = storage_dir.join(&victim_hex);
    assert!(out_dir.exists(), "victim data on disk after phase 1");
    engine.close().await;

    // Phase 2: the lapsed probe is listed too. It needs 51.2 MB + buffer
    // against ~50 MB free, so only a swap could fit it — and a swap would
    // be legal on margin (probe 5 seeders vs victim 10 with margin 4), so
    // the probe guard is the only thing standing between it and the victim.
    {
        let mut st = keep_at::state::State::load(&data_dir.join("state.json")).expect("state");
        st.quarantine_put(
            probe_hex.clone(),
            lapsed_quarantine_entry(&probe_fx.title, 1),
        )
        .expect("registry seed");
    }
    let (cat2, _s2) = common::serve_catalog(Stub::catalog_xml(&rows));
    std::mem::forget(_s2);
    let mut cfg2 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(99),
        150_000_000,
    );
    cfg2.aggressiveness = 0.999999;
    cfg2.scan.interval = Duration::from_secs(3600);
    cfg2.scan.quarantine_check_interval = Duration::from_secs(60);
    cfg2.scan.min_seed_margin = 4;
    let mut engine2 =
        keep_at::engine::Engine::new_with_options(cfg2, test_options(&cat2, &stub.base_url))
            .await
            .expect("engine2 new");

    let (_sd_tx2, sd_rx2) = tokio::sync::watch::channel(false);
    engine2.quarantine_pass(&sd_rx2).await;

    let held: Vec<String> = engine2
        .held_torrents()
        .into_iter()
        .map(|t| t.info_hash)
        .collect();
    assert!(
        held.contains(&victim_hex),
        "the healthy held torrent must survive a timer-driven re-probe (held: {held:?})"
    );
    assert!(
        !held.contains(&probe_hex),
        "the probe must be deferred, not added via swap — probes never displace          held torrents, on the timer path as on the scan path (held: {held:?})"
    );
    assert!(
        out_dir.exists(),
        "victim data intact — no displacement deleted it"
    );

    let st = keep_at::state::State::load(&data_dir.join("state.json")).expect("state");
    assert_eq!(
        st.quarantine_get(&probe_hex)
            .expect("entry survives")
            .attempts,
        1,
        "a deferred probe must not count as an attempt"
    );
    engine2.close().await;
}
