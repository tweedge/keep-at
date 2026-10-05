//! Swap economics: margin enforcement, disk+RAM coverage, eviction
//! order mirroring ranking, file removal, and exactly one displacement log
//! line per evicted torrent.
//!
//! The stub serves one tracker per torrent with canned counts; the gate is
//! forced open so every candidate reaches the swap path on its merits
//! (margin + coverage), not on a roll.

mod common;

use std::collections::HashMap;

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub};

fn setup(fixtures: &[Fixture]) -> (Stub, String, Vec<String>) {
    let (stub, catalog_base, hexes, _) = common::setup_with_catalog(fixtures);
    (stub, catalog_base, hexes)
}

#[tokio::test(flavor = "multi_thread")]
async fn swap_beats_margin_and_covers_cost() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    // Held: one 10-seeder, 100 MB torrent (1 piece). Candidate: 5-seeder,
    // 51.2 MB (1 piece). Margin 2: 5 <= 10-2 passes. Disk: the candidate
    // fits in the 150 MB location only by displacing (100 + 51.2 MB +
    // buffers > 150 MB). RAM: trivially covered.
    let held_fx = Fixture::new("held-big", 100_000_000, 10);
    let cand_fx = Fixture::new("cand-small", 51_200_000, 5);
    let (stub, _catalog_base, hexes) = setup(&[held_fx.clone(), cand_fx.clone()]);
    assert_eq!(hexes.len(), 2);
    let (held_hex, cand_hex) = (hexes[0].clone(), hexes[1].clone());

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();

    // Phase 1: scan with ONLY the held torrent in the catalog -> holds it.
    let (cat1, _s1) = common::serve_catalog(Stub::catalog_xml(&[(
        held_fx.title.clone(),
        held_hex.clone(),
        held_fx.size,
    )]));
    std::mem::forget(_s1);
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(21),
        150_000_000,
    );
    cfg.scan.min_seed_margin = 2;
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
    let held: HashMap<_, _> = engine
        .held_torrents()
        .into_iter()
        .map(|t| (t.info_hash.clone(), t))
        .collect();
    assert!(
        held.contains_key(&held_hex),
        "phase 1 holds the big torrent"
    );
    assert_eq!(held.len(), 1);
    // Piece count was recorded from metainfo (1 piece for 1 MB / 1 MB pieces).
    assert_eq!(held[&held_hex].piece_count, 1);
    engine.close().await;

    // History: the fill add is recorded with its seed-scarcity statistics
    // and no displaced set.
    let (events, skipped) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
    assert_eq!(skipped, 0);
    assert_eq!(events.len(), 1);
    match &events[0] {
        keep_at::history::Event::Add {
            hash,
            title,
            cause,
            chance,
            roll,
            reason,
            displaced,
            ..
        } => {
            assert_eq!(hash, &held_hex);
            assert_eq!(title, &held_fx.title);
            assert_eq!(cause, &keep_at::history::Cause::Fill);
            assert!(displaced.is_empty(), "fills carry no displaced set");
            assert!(*chance > 0.0 && *roll >= 0.0, "scarcity stats recorded");
            assert!(!reason.is_empty(), "admission reason recorded");
        }
        _ => panic!("expected a fill Add event"),
    }

    // Phase 2: catalog now has BOTH; the candidate must displace the held
    // one (disk: 1 MB held + 512 KB candidate > 1 MB limit).
    let (cat2, _s2) = common::serve_catalog(Stub::catalog_xml(&[
        (held_fx.title.clone(), held_hex.clone(), held_fx.size),
        (cand_fx.title.clone(), cand_hex.clone(), cand_fx.size),
    ]));
    std::mem::forget(_s2);
    let cfg2 = {
        let mut c = test_config(
            data_dir.clone(),
            storage_dir.clone(),
            test_port(22),
            150_000_000,
        );
        c.scan.min_seed_margin = 2;
        c
    };
    // Same data dir (state persists), new engine on a fresh port.
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

    let held2: HashMap<_, _> = engine2
        .held_torrents()
        .into_iter()
        .map(|t| (t.info_hash.clone(), t))
        .collect();
    assert!(
        held2.contains_key(&cand_hex),
        "candidate displaced the held torrent"
    );
    assert!(
        !held2.contains_key(&held_hex),
        "displaced torrent no longer held"
    );
    // Displaced files removed from disk.
    assert!(
        !storage_dir.join(&held_hex).exists(),
        "displaced output dir removed"
    );
    // Winner's files exist.
    assert!(
        storage_dir.join(&cand_hex).exists(),
        "winner output dir exists"
    );

    // History: the swap is one Add event carrying its displaced set — no
    // separate Remove rows for swap evictions.
    let (events, _) = keep_at::history::read_events(&data_dir.join("history.jsonl"));
    assert_eq!(events.len(), 2);
    match &events[1] {
        keep_at::history::Event::Add {
            hash,
            cause,
            displaced,
            ..
        } => {
            assert_eq!(hash, &cand_hex);
            assert_eq!(cause, &keep_at::history::Cause::Swap);
            assert_eq!(displaced.len(), 1);
            assert_eq!(displaced[0].hash, held_hex);
            assert_eq!(displaced[0].title, held_fx.title);
        }
        _ => panic!("expected a swap Add event"),
    }
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, keep_at::history::Event::Remove { .. })),
        "swap evictions are recorded in the winner's displaced set, not as drops"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn margin_blocks_weak_candidate() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    // Held 10-seeder; candidate 9-seeder; margin 2: 9 <= 10-2 = 8 fails.
    // Disk fits both (2 MB limit), so any add would be free-space — but the
    // gate is forced open and free-space... note free-space fill does NOT
    // check the margin (nothing displaced). To isolate the margin, the
    // location must be FULL: held torrent exactly fills it.
    let held_fx = Fixture::new("held-full", 100_000_000, 10);
    let cand_fx = Fixture::new("cand-weak", 10_000_000, 9);
    let (stub, _catalog_base, hexes) = setup(&[held_fx.clone(), cand_fx.clone()]);
    let (held_hex, cand_hex) = (hexes[0].clone(), hexes[1].clone());

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();

    // Phase 1 holds the 100 MB torrent in a ~100.3 MB location (full: no room for even the buffer of another).
    let (cat1, _s1) = common::serve_catalog(Stub::catalog_xml(&[(
        held_fx.title.clone(),
        held_hex.clone(),
        held_fx.size,
    )]));
    std::mem::forget(_s1);
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(23),
        105_000_000,
    );
    cfg.scan.min_seed_margin = 2;
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

    // Phase 2: weak candidate cannot displace (margin), cannot fit
    // free-space (location full: 100 MB held + 10 MB > ~100.3 MB limit).
    let (cat2, _s2) = common::serve_catalog(Stub::catalog_xml(&[
        (held_fx.title.clone(), held_hex.clone(), held_fx.size),
        (cand_fx.title.clone(), cand_hex.clone(), cand_fx.size),
    ]));
    std::mem::forget(_s2);
    let mut cfg2 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(24),
        105_000_000,
    );
    cfg2.scan.min_seed_margin = 2;
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

    let held2: HashMap<_, _> = engine2
        .held_torrents()
        .into_iter()
        .map(|t| (t.info_hash.clone(), t))
        .collect();
    assert!(held2.contains_key(&held_hex), "held survives");
    assert!(
        !held2.contains_key(&cand_hex),
        "weak candidate rejected by margin"
    );
}

/// Regression: a swap is 1-in / N-out, and the engine's running
/// `held_count` has to follow it. The count gates the free-space fill path
/// (`held_count < max_torrents`), so leaving it over-counted after a swap
/// that displaced more than it added made later candidates in the SAME scan
/// skip fill and walk the swap path - where they are correctly refused, so
/// the node simply stops taking free room until the next scan re-derives
/// the count from state.
///
/// Setup pins `max_torrents` to 2 (via a 1 MiB RAM budget) and fills both
/// slots, then hands the scan one big candidate that only fits by displacing
/// BOTH held torrents (2-out, 1-in -> net 1) followed by a small one that
/// plainly fits in the free space that swap created. The small candidate is
/// seeded too weakly to displace anything the big one left behind, so if
/// the fill gate has drifted shut it cannot get in at all.
#[tokio::test(flavor = "multi_thread")]
async fn multi_displacement_swap_keeps_fill_gate_honest() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();

    // 400 KiB each + the 256 KiB per-torrent buffer: two of them exactly
    // fill a 1_324_288-byte location, so there is no free space to start.
    let held_a = Fixture::new("held-a", 400_000, 30);
    let held_b = Fixture::new("held-b", 400_000, 30);
    // Ranked first (fewest seeders), sized so it needs BOTH held torrents
    // freed: 300_000 + 262_144 = 562_144 > one 400_000, <= two.
    let cand_big = Fixture::new("cand-big", 300_000, 5);
    // Ranked second, sized to fit the free space the swap leaves behind.
    let cand_small = Fixture::new("cand-small", 100_000, 6);

    let (stub, _catalog_base, hexes) = setup(&[
        held_a.clone(),
        held_b.clone(),
        cand_big.clone(),
        cand_small.clone(),
    ]);
    let (held_a_hex, held_b_hex, big_hex, small_hex) = (
        hexes[0].clone(),
        hexes[1].clone(),
        hexes[2].clone(),
        hexes[3].clone(),
    );

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();

    // Phase 1: hold both torrents (2 of 2 slots).
    let (cat1, _s1) = common::serve_catalog(Stub::catalog_xml(&[
        (held_a.title.clone(), held_a_hex.clone(), held_a.size),
        (held_b.title.clone(), held_b_hex.clone(), held_b.size),
    ]));
    std::mem::forget(_s1);
    // The location limit only has to be legal (>= 100M); free space is
    // irrelevant here because the torrent CAP closes the fill path before
    // choose_location is ever consulted.
    let location_limit = 200_000_000u64;
    let mut cfg = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(23),
        location_limit,
    );
    cfg.scan.min_seed_margin = 4;
    // Force the seed-scarcity gate open: this test is about placement
    // bookkeeping, not roll odds.
    cfg.aggressiveness = 0.999999;
    // 1 MiB budget / 512 KiB typical -> max_torrents = 2. Both slots fill
    // in phase 1, so phase 2's fill path starts closed.
    cfg.max_ram = 1024 * 1024;
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
    assert_eq!(engine.held_torrents().len(), 2, "phase 1 fills both slots");
    engine.close().await;

    // Phase 2: all four in the catalog. The big candidate swaps both out;
    // the small one must then FILL the freed space.
    let (cat2, _s2) = common::serve_catalog(Stub::catalog_xml(&[
        (held_a.title.clone(), held_a_hex.clone(), held_a.size),
        (held_b.title.clone(), held_b_hex.clone(), held_b.size),
        (cand_big.title.clone(), big_hex.clone(), cand_big.size),
        (cand_small.title.clone(), small_hex.clone(), cand_small.size),
    ]));
    std::mem::forget(_s2);
    let mut cfg2 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(24),
        location_limit,
    );
    cfg2.scan.min_seed_margin = 4;
    cfg2.aggressiveness = 0.999999;
    cfg2.max_ram = 1024 * 1024;
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

    let held = engine2.held_torrents();
    let held_hexes: Vec<&str> = held.iter().map(|t| t.info_hash.as_str()).collect();
    assert!(
        held_hexes.contains(&big_hex.as_str()),
        "the big candidate must displace both held torrents and be held ({held_hexes:?})"
    );
    assert!(
        held_hexes.contains(&small_hex.as_str()),
        "BUG: the small candidate fits the free space the swap created but was not \
         held - the fill gate drifted shut after a 2-out/1-in swap \
         (held_count was left over-counted). held: {held_hexes:?}"
    );
    engine2.close().await;
}
