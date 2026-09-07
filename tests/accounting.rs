//! Nominal-plus-buffer accounting: free space prices held NOMINAL sizes
//! plus per-torrent buffers, never on-disk actuals.
//!
//! Regression net for the production over-commit defect (4.17 TB nominal
//! held against a 3.8 TB limit while actual disk sat at 277 GB): downloads
//! land sparse, so on-disk actuals lag nominal by orders of magnitude early
//! on, and pricing actuals lets the node over-commit without bound.

mod common;

use std::sync::{Arc, Mutex};

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub, StubState};

#[tokio::test(flavor = "multi_thread")]
async fn free_space_prices_nominal_not_actual() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    // Two 600 KB torrents, 1 MB location (+256 KiB buffer each):
    // first fits (600K+256K < 1MB), second needs 600K+256K > ~200K free.
    // On-disk actuals are ~zero (sparse, nothing downloaded yet) — an
    // actuals-pricing implementation would hold both; nominal pricing
    // holds exactly one.
    let fixtures = vec![
        Fixture::new("first-600k", 600_000, 1),
        Fixture::new("second-600k", 600_000, 1),
    ];
    let state = Arc::new(Mutex::new(StubState::default()));
    let mut raws = Vec::new();
    for f in &fixtures {
        raws.push((
            f.clone(),
            common::torrent_bytes(f, "http://127.0.0.1:9/announce"),
        ));
    }
    let stub = Stub::start(Stub::catalog_xml(&[]), state.clone());
    let tracker = stub.tracker_url();
    let mut rows = Vec::new();
    {
        let mut st = state.lock().unwrap();
        for (f, _) in &raws {
            let (hex, _) = st.add(f, &tracker);
            rows.push((f.title.clone(), hex, f.size));
        }
    }
    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&rows));
    std::mem::forget(_srv);

    let data_dir = tempfile::tempdir().unwrap().keep();
    let storage_dir = tempfile::tempdir().unwrap().keep();
    let cfg = test_config(data_dir.clone(), storage_dir, test_port(31), 1_000_000);
    let mut engine = with_timeout(
        60,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url)),
    )
    .await
    .expect("engine new");
    with_timeout(180, "scan", engine.scan_once())
        .await
        .expect("scan");
    engine.close().await;

    let held = engine.held_torrents();
    assert_eq!(
        held.len(),
        1,
        "nominal accounting holds exactly one (actuals-pricing would hold both)"
    );
    // And the survivor fully covers its nominal: state nominal <= limit.
    let nominal: u64 = held.iter().map(|t| t.size_bytes).sum();
    assert!(nominal <= 1_000_000, "held nominal within limit");
}
