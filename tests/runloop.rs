//! Engine::run loop mechanics, exercised end-to-end against the stub:
//!
//! - a pending next-scan delay is honored: periodic ticks fire INSIDE the
//!   wait and must not end it (the old bare-select bug scanned ~30min into
//!   a multi-day wait — live incidents on the Sep 21 + Sep 26 boots);
//! - a due-now boot (interrupted-scan recovery: no completed-scan stamp)
//!   scans immediately;
//! - a completed scan is paced, never re-run immediately (the deadline
//!   reschedule floors at 60s, matching the old interval ticker).
//!
//! The first test is deliberately slow (~85s): the stats ticker floors at
//! 60s, so the window where a tick could end the wait needs a >60s delay
//! to be decisive.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub, StubState};

// The seeder session disables DHT (src/engine/session.rs: `dht: None` —
// the Sep 2026 memory fix), so sessions share no dht.json and race on no
// UDP port; other shared surfaces (test ports, catalog servers on random
// ports) are already disjoint across this file's two tests.
static DHT_STATE: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// One fixture listed in a stub catalog. Returns (stub, catalog_base, hex).
fn setup_one(title: &str) -> (Stub, String, String) {
    let fx = Fixture::new(title, 50_000, 4);
    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let tracker = stub.tracker_url();
    let hex = {
        let mut st = state.lock().unwrap();
        st.add(&fx, &tracker).0
    };
    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&[(
        fx.title.clone(),
        hex.clone(),
        fx.size,
    )]));
    std::mem::forget(_srv);
    (stub, catalog_base, hex)
}

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

// Engine is not Send (a non-Send rng rides inside its reqwest client), so
// the run loop is driven on a LocalSet instead of tokio::spawn — same
// constraint the scan_once tests avoid by awaiting inline.
fn drive_run(
    mut engine: keep_at::engine::Engine,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::task::spawn_local(async move { engine.run(shutdown_rx).await })
}

#[tokio::test(flavor = "current_thread")]
async fn pending_delay_survives_ticks_and_deadline_fires_the_scan() {
    tokio::task::LocalSet::new()
        .run_until(pending_delay_survives_ticks_body())
        .await;
}

async fn pending_delay_survives_ticks_body() {
    let _dht_guard = DHT_STATE.lock().await;
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let (stub, catalog_base, _hex) = setup_one("delayed-run");
    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();

    // Last scan "completed" now with a 75s interval: the next scan is due
    // ~75s out. The stats ticker (60s floor) fires once inside that wait —
    // the buggy bare-select ended the wait on exactly that tick.
    seed_completed_scan(&data_dir, chrono::Utc::now());
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(91),
        1 << 30,
    );
    cfg.scan.interval = Duration::from_secs(75);
    cfg.stats_interval = Duration::from_secs(1);
    cfg.scan.quarantine_check_interval = Duration::ZERO;

    let engine =
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url))
            .await
            .expect("engine new");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let run_task = drive_run(engine, shutdown_rx);

    // Sampled INSIDE the wait window (deadline ≈ 72-74s out): the buggy
    // bare-select ended the wait on the FIRST stats tick (~1s in, the
    // test sets stats_interval=1s), so a scan would already hold the
    // fixture by 40s. 40s (not a later sample) keeps ~30s of margin to
    // the deadline — a wake-late under CI load cannot cross it.
    tokio::time::sleep(Duration::from_secs(40)).await;
    assert!(
        common::read_held(&data_dir).is_empty(),
        "a periodic tick must not end the post-boot wait"
    );
    let snap = keep_at::netstats::load_snapshot(&data_dir.join("network-stats.json")).unwrap();
    assert!(
        snap.scan_started_at
            .is_some_and(|t| chrono::Utc::now() - t > chrono::Duration::try_minutes(1).unwrap()),
        "no scan may start during the wait (scan_started_at must be the seeded one)"
    );

    // The deadline fires: the initial scan runs and holds the fixture.
    let poll_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if !common::read_held(&data_dir).is_empty() {
            break;
        }
        assert!(
            Instant::now() < poll_deadline,
            "deadline did not fire the initial scan"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    shutdown_tx.send(true).unwrap();
    with_timeout(30, "run shutdown", run_task)
        .await
        .expect("run task joins")
        .expect("run returns Ok");
}

#[tokio::test(flavor = "current_thread")]
async fn due_now_scans_immediately_and_is_paced_afterwards() {
    tokio::task::LocalSet::new()
        .run_until(due_now_scans_immediately_body())
        .await;
}

async fn due_now_scans_immediately_body() {
    let _dht_guard = DHT_STATE.lock().await;
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let (stub, catalog_base, _hex) = setup_one("due-now-run");
    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();

    // No completed-scan stamp: delay = 0 → the initial scan runs at once
    // (this is the interrupted-scan recovery path the Sep 26 repair used).
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(92),
        1 << 30,
    );
    cfg.scan.interval = Duration::from_secs(1);
    cfg.scan.quarantine_check_interval = Duration::ZERO;

    let engine =
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url))
            .await
            .expect("engine new");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let run_task = drive_run(engine, shutdown_rx);

    let poll_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if !common::read_held(&data_dir).is_empty() {
            break;
        }
        assert!(
            Instant::now() < poll_deadline,
            "due-now boot must scan immediately"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Reschedule after a completed scan: interval (1s) floored to 60s —
    // the immediate re-run of the old back-to-back bug must not happen.
    // Held-refresh scrapes only happen inside scans, so stable scrape
    // hits over a window prove no second scan ran.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let hits_a = stub.state.lock().unwrap().scrape_hits.len();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let hits_b = stub.state.lock().unwrap().scrape_hits.len();
    assert_eq!(
        hits_a, hits_b,
        "a completed scan must not be immediately re-run"
    );

    shutdown_tx.send(true).unwrap();
    with_timeout(30, "run shutdown", run_task)
        .await
        .expect("run task joins")
        .expect("run returns Ok");
}
