//! Regression: a catalog that PARSES but yields zero items (AT schema drift
//! dropping every infohash, or an empty-but-well-formed channel) used to be
//! treated as authoritative ground truth, wiping the entire held library and
//! deleting the downloaded files (0.8.23 and earlier). The collapse guard
//! now refuses the removed-from-catalog eviction pass unless the fresh
//! catalog lists at least catalog_collapse_percent% of the held set.

mod common;

use std::sync::{Arc, Mutex};

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub, StubState};

/// The two tests below each build a real Engine, and the production session
/// options keep rqbit's DHT enabled with persistence (default): both DHT
/// instances bind the same persisted UDP port and read/write the same
/// `~/.cache/com.rqbit.dht/dht.json`. Run concurrently (the default within
/// one test binary) that races: observed as a ~1-in-5 full-suite failure of
/// these tests with corrupted shared DHT state. Serializing just this file
/// removes the race without serializing the whole suite.
static DHT_STATE: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

fn data_files_recursive(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// A poisoned catalog: parses cleanly, but every infohash fails hex/length,
/// so the item list is empty while the fetch itself succeeded.
fn poisoned_catalog() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel>
<item><title>Schema Drift</title><category>x</category><infohash>NOT-A-HASH</infohash><guid>g</guid><link>l</link><description>d</description><size>100000</size></item>
</channel></rss>"#
        .to_string()
}

async fn hold_one_fixture(
    port: u16,
    data_dir: &std::path::Path,
    storage_dir: &std::path::Path,
    title: &str,
) {
    let fx = Fixture::new(title, 100_000, 4);
    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let hex = {
        let mut st = state.lock().unwrap();
        st.add(&fx, &stub.tracker_url()).0
    };

    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&[(
        "doomed".to_string(),
        hex.clone(),
        100_000,
    )]));
    std::mem::forget(_srv);

    let cfg = test_config(
        data_dir.to_path_buf(),
        storage_dir.to_path_buf(),
        port,
        1 << 30,
    );
    let mut engine = with_timeout(
        60,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(120, "scan 1", engine.scan_once())
        .await
        .expect("scan 1");
    assert_eq!(engine.held_torrents().len(), 1, "fixture held after scan 1");
    engine.close().await;
    let _ = hex;
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_catalog_response_preserves_held_library() {
    let _dht_guard = DHT_STATE.lock().await;
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    hold_one_fixture(test_port(71), &data_dir, &storage_dir, "doomed-a").await;

    let held_hash = engine_held_hash(&data_dir).expect("held hash present");
    let out_dir = storage_dir.join(&held_hash);
    assert!(
        !data_files_recursive(&out_dir).is_empty(),
        "sanity: seeded data exists at {out_dir:?}"
    );

    // Phase 2: zero-item catalog, default guard (30%) -> library preserved.
    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let (poison_base, _srv2) = common::serve_catalog(poisoned_catalog());
    std::mem::forget(_srv2);
    let cfg2 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(72),
        1 << 30,
    );
    let mut engine2 = with_timeout(
        60,
        "engine2 new",
        keep_at::engine::Engine::new_with_options(cfg2, test_options(&poison_base, &stub.base_url)),
    )
    .await
    .expect("engine2 new");
    with_timeout(120, "scan 2", engine2.scan_once())
        .await
        .expect("scan 2");

    let held_after = engine2.held_torrents();
    let after_files = data_files_recursive(&out_dir);
    if held_after.len() != 1 || after_files.is_empty() {
        eprintln!(
            "FLAKE DEBUG: held={held_after:?} state.json={} storage listing={:?} out_dir={} exists={}",
            std::fs::read_to_string(data_dir.join("state.json")).unwrap_or_default(),
            std::fs::read_dir(&storage_dir)
                .map(|rd| rd.flatten().map(|e| e.path().display().to_string()).collect::<Vec<_>>())
                .unwrap_or_default(),
            out_dir.display(),
            out_dir.exists(),
        );
    }
    assert_eq!(
        held_after.len(),
        1,
        "collapse guard must preserve the held library"
    );
    assert!(
        !after_files.is_empty(),
        "collapse guard must preserve downloaded data"
    );
    engine2.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn collapse_guard_zero_disables_and_removal_proceeds() {
    let _dht_guard = DHT_STATE.lock().await;
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    hold_one_fixture(test_port(73), &data_dir, &storage_dir, "doomed-b").await;

    let state = Arc::new(Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let (poison_base, _srv2) = common::serve_catalog(poisoned_catalog());
    std::mem::forget(_srv2);
    let mut cfg3 = test_config(
        data_dir.clone(),
        storage_dir.clone(),
        test_port(74),
        1 << 30,
    );
    cfg3.catalog_collapse_percent = 0; // operator disables the guard
    let mut engine3 = with_timeout(
        60,
        "engine3 new",
        keep_at::engine::Engine::new_with_options(cfg3, test_options(&poison_base, &stub.base_url)),
    )
    .await
    .expect("engine3 new");
    with_timeout(120, "scan", engine3.scan_once())
        .await
        .expect("scan");

    // Old behavior, explicitly opted into: the removal pass runs.
    assert_eq!(
        engine3.held_torrents().len(),
        0,
        "guard disabled: removal proceeds as before"
    );
    engine3.close().await;
}

/// Read the single held hash out of state.json (phase 1 wrote exactly one).
fn engine_held_hash(data_dir: &std::path::Path) -> Option<String> {
    let data = std::fs::read_to_string(data_dir.join("state.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    v.get("torrents")?.as_object()?.keys().next().cloned()
}
