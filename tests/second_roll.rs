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
// The other half of the same invariant: the roll is drawn ONCE per
// evaluation, even when the fill path is never attempted. Reaching the swap
// path without having consumed a roll used to hand the candidate one fresh
// roll per storage location (P(admit) = 1-(1-p)^k), so a multi-location node
// at the torrent cap admitted candidates far more often than the documented
// chance - and displaced better-seeded held torrents to do it.
// ---------------------------------------------------------------------------

/// Held torrents priced to make every swap RAM-neutral: same piece count as
/// the candidates (so `freed_ram == ram_cost` and the RAM headroom stays
/// pinned at zero), but larger on disk (so one displacement covers the
/// candidate's nominal+buffer need). Written straight to state.json - the
/// regression is about placement policy, not about the download pipeline.
fn held_giant(
    info_hash: &str,
    title: &str,
    loc: &std::path::Path,
    seeders: u32,
) -> keep_at::state::Torrent {
    keep_at::state::Torrent {
        info_hash: info_hash.to_string(),
        title: title.to_string(),
        size_bytes: 512_000,
        storage_location: loc.to_path_buf(),
        added_at: chrono::Utc::now(),
        piece_count: 1024,
        last_known_seeders: seeders,
        completed_pieces: 0,
        last_progress_at: Some(chrono::Utc::now()),
        last_confirmed_in_catalog_at: Some(chrono::Utc::now()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn at_cap_candidate_gets_one_roll_not_one_per_location() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("ADV_LOG").unwrap_or_else(|_| "info".into()))
        .try_init();

    // Two storage locations, each deep enough in held giants that the
    // displaceable set never runs dry inside the candidate count below -
    // so `try_swap` always has BOTH locations available and, under the old
    // code, always drew two independent rolls per candidate.
    let n_candidates: usize = 200;
    let per_location: usize = 200;

    let data_dir = tempfile::tempdir().unwrap().keep();
    let loc_a = tempfile::tempdir().unwrap().keep();
    let loc_b = tempfile::tempdir().unwrap().keep();

    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let tracker = stub.tracker_url();

    // Giants: piece_count 1024 -> RAM price identical to the candidates
    // (RAM-neutral swaps), size 512000 -> one displacement covers the
    // candidate's 102400 + 256KiB buffer need.
    let mut giant_rows: Vec<(String, String, u64)> = Vec::new();
    {
        let mut st = state.lock().unwrap();
        for (loc, loc_idx) in [(&loc_a, 0usize), (&loc_b, 1usize)] {
            for j in 0..per_location {
                let title = format!("giant-{loc_idx}-{j}");
                let mut fx = Fixture::new(&title, 512_000, 30);
                fx.piece_len = 500; // 512000 / 500 = 1024 pieces
                let (hex, _raw) = st.add(&fx, &tracker);
                giant_rows.push((title.clone(), hex.clone(), fx.size));
                let t = held_giant(&hex, &title, loc, 30);
                let mut s =
                    keep_at::state::State::load(&data_dir.join("state.json")).expect("state loads");
                s.put(t).expect("seed held giant");
            }
        }
    }

    // Candidates: piece_count 1024 (same RAM price), size 102400.
    let mut cand_rows: Vec<(String, String, u64)> = Vec::new();
    {
        let mut st = state.lock().unwrap();
        for j in 0..n_candidates {
            let title = format!("cand-{j}");
            let mut fx = Fixture::new(&title, 102_400, 21);
            fx.piece_len = 100; // 102400 / 100 = 1024 pieces
            let (hex, _raw) = st.add(&fx, &tracker);
            cand_rows.push((title, hex, fx.size));
        }
    }

    let mut all_rows = giant_rows.clone();
    all_rows.extend(cand_rows.iter().cloned());
    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&all_rows));
    std::mem::forget(_srv);

    // Seeder floor pinned at 20: candidates at 21 -> chance = 0.5^(21-20)
    // = 0.5. Giants at 30 clear the default 4-seed margin against 21.
    keep_at::netstats::save_snapshot(
        &data_dir.join("network-stats.json"),
        &keep_at::netstats::Snapshot {
            scan_started_at: Some(chrono::Utc::now()),
            scan_completed_at: Some(chrono::Utc::now()),
            total_candidates: 0,
            processed_candidates: 0,
            seeder_floor: 20,
        },
    )
    .expect("seed floor");

    let mut cfg = test_config(
        data_dir.clone(),
        loc_a.clone(),
        test_port(67),
        10_000_000_000,
    );
    // Second location, same generous limit.
    cfg.storage.push(keep_at::config::StorageLocation {
        path: loc_b.clone(),
        limit: keep_at::config::StorageLimit::Bytes(10_000_000_000),
    });
    cfg.aggressiveness = 0.5;
    // Budget sized so that the 400 giants (512 KiB RAM each at the 4-peer
    // limit of a small budget) exactly cover it, and max_torrents lands at
    // 400 - i.e. the node is simultaneously at the torrent cap and out of
    // RAM headroom, so `act_on_candidate` skips the fill path entirely and
    // never draws a roll there. Swaps stay RAM-neutral, so both stay true
    // for the whole run.
    cfg.max_ram = 200 * 1024 * 1024;

    let mut engine = with_timeout(
        300,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(600, "scan", engine.scan_once())
        .await
        .expect("scan");
    engine.close().await;

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

    // One roll per candidate at p = 0.5 over 200 candidates: mean 100,
    // sd 7.07. Two independent rolls per candidate (the bug) is
    // p = 1-(1-0.5)^2 = 0.75: mean 150, sd 6.12. The band below sits
    // 2.8 sd above the correct mean and ~5 sd below the buggy mean, so it
    // fails the bug essentially always and the correct code essentially
    // never.
    assert!(
        swaps.len() <= 120,
        "BUG: {} candidates admitted via swap - consistent with TWO independent \
         scarcity rolls per candidate (expected ~75 under the bug, ~50 with one \
         roll). Rolls must be drawn once per evaluation, not once per location: {:?}",
        swaps.len(),
        swaps
    );
    assert!(
        swaps.len() >= 25,
        "sanity: with p=0.5 over {n_candidates} candidates some swaps are expected \
         (got {}) - if this is 0 the gate is closed and the test is vacuous",
        swaps.len()
    );
}

// ---------------------------------------------------------------------------
// ATTACK C (fixed): at size_bias == 0 the eviction order used to invert
// urgency - the ranking's most-urgent (fewest-seeder) held torrent was the
// FIRST evicted, contradicting the documented legacy order (highest-seeded
// evicted first). The invariant now lives as a unit test next to the code
// that owns it: selector::tests::zero_bias_evicts_highest_seeded_first.
// ---------------------------------------------------------------------------
