//! Regression: a candidate that LOSES its seed-scarcity roll on the fill
//! path must be skipped entirely (DESIGN.md) - it must not get a second
//! roll per storage location through the swap path, displacing
//! strictly better-seeded held torrents while free space is plentiful.

mod common;

use std::sync::{Arc, Mutex};

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub, StubState};

#[tokio::test(flavor = "multi_thread")]
async fn failed_fill_roll_must_not_get_a_second_chance_via_swap() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("ADV_LOG").unwrap_or_else(|_| "info".into()))
        .try_init();
    // Phase 0 seeds seeder_floor = 1 in network-stats.json (50 one-seeder
    // fillers, chance 1.0 at floor 0). Phase 1 uses seeders-2 candidates:
    // chance = 0.5^(2-1) = 0.5, so roughly half fail the fill roll. Storage
    // holds EVERYTHING, so no swap is ever necessary: any Swap event proves
    // a failed-fill candidate got a second roll via try_swap and displaced
    // a well-seeded held torrent although free space was plentiful.
    let n = 50;
    let filler_fx: Vec<Fixture> = (0..n)
        .map(|i| Fixture::new(&format!("filler-{i}"), 100_000, 1))
        .collect();
    let held_fx: Vec<Fixture> = (0..n)
        .map(|i| Fixture::new(&format!("held-{i}"), 10_000_000, 10))
        .collect();
    let cand_fx: Vec<Fixture> = (0..n)
        .map(|i| Fixture::new(&format!("cand-{i}"), 100_000, 2))
        .collect();

    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let tracker = stub.tracker_url();

    // Register everything under the stub; remember hash per title.
    let raws: Vec<(Fixture, Vec<u8>)> = filler_fx
        .iter()
        .chain(held_fx.iter())
        .chain(cand_fx.iter())
        .map(|f| {
            let raw = common::torrent_bytes(f, &tracker);
            (f.clone(), raw)
        })
        .collect();
    {
        let mut st = state.lock().unwrap();
        for (f, raw) in &raws {
            let hex = common::torrent_info_hash(raw);
            st.torrents.insert(hex.clone(), raw.clone());
            st.scrapes.insert(hex, (f.seeders, f.leechers));
        }
    }
    let catalog_of = |names: Vec<String>| {
        let rows: Vec<(String, String, u64)> = raws
            .iter()
            .filter(|(f, _)| names.contains(&f.title))
            .map(|(f, raw)| (f.title.clone(), common::torrent_info_hash(raw), f.size))
            .collect();
        let (base, srv) = common::serve_catalog(Stub::catalog_xml(&rows));
        std::mem::forget(srv);
        base
    };

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();

    // Phase 0: fillers + the 10-seeder pool, gate open (0.999999) so the
    // well-seeded torrents are HELD, and the post-scan seeder floor is
    // anchored at 1 by the one-seeder fillers.
    let cat0 = catalog_of(
        filler_fx
            .iter()
            .chain(held_fx.iter())
            .map(|f| f.title.clone())
            .collect(),
    );
    let cfg0 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(65),
        5_000_000_000,
    );
    let mut engine0 = with_timeout(
        300,
        "engine0 new",
        keep_at::engine::Engine::new_with_options(cfg0, test_options(&cat0, &stub.base_url)),
    )
    .await
    .expect("engine0 new");
    with_timeout(300, "scan 0", engine0.scan_once())
        .await
        .expect("scan 0");
    engine0.close().await;
    let snap = common::read_snapshot(&data_dir);
    assert_eq!(snap.seeder_floor, 1, "phase 0 anchors floor at 1");

    // Phase 1: the 10-seeders are already held (and in the catalog, so they
    // survive remove_deleted); 2-seeder candidates get chance 0.5, margin 2,
    // and free space stays ample throughout.
    let cat1 = catalog_of(
        held_fx
            .iter()
            .chain(cand_fx.iter())
            .map(|f| f.title.clone())
            .collect(),
    );
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(66),
        5_000_000_000,
    );
    cfg.aggressiveness = 0.5;
    cfg.scan.min_seed_margin = 2;
    let mut engine = with_timeout(
        300,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&cat1, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(600, "scan 1", engine.scan_once())
        .await
        .expect("scan 1");
    engine.close().await;

    let held = engine.held_torrents();
    let fills = held.iter().filter(|t| t.title.starts_with("cand-")).count();
    let (events, _) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
    let swaps: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            keep_at::history::Event::Add {
                title,
                cause: keep_at::history::Cause::Swap,
                ..
            } => Some(title.clone()),
            _ => None,
        })
        .collect();
    assert!(
        fills > 0,
        "sanity: some candidates should fill free space (gate p=0.5 over {n})"
    );
    assert!(
        swaps.is_empty(),
        "BUG: {} candidates displaced well-seeded held torrents via a SECOND          scarcity roll although free space was plentiful for a plain fill: {:?}",
        swaps.len(),
        swaps
    );
}

// ---------------------------------------------------------------------------
// ATTACK C: at size_bias == 0 the eviction order inverts urgency: the
// ranking's most-urgent (fewest-seeder) held torrent is the FIRST evicted,
// contradicting both the documented legacy order (highest-seeded evicted
// first) and the rank<->eviction mirror invariant.
// ---------------------------------------------------------------------------
