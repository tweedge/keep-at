//! Live query socket: protocol round-trip plus a real Engine serving.
//!
//! Asserts through the public surface only: request/response JSON shapes,
//! offline fallback (no socket => None), and — with a stub-backed Engine
//! that holds fixtures — a live `serve` + `query` round-trip whose numbers
//! match the Engine's own held set.

mod common;

use std::sync::{Arc, Mutex};

use common::{test_config, test_options, test_port, with_timeout, Fixture, Stub, StubState};

#[test]
fn protocol_shapes() {
    // Request serializes as the documented one-line verbs.
    assert_eq!(
        serde_json::to_string(&keep_at::live::Request::Runtime).unwrap(),
        r#"{"op":"runtime"}"#
    );
    assert_eq!(
        serde_json::to_string(&keep_at::live::Request::Held).unwrap(),
        r#"{"op":"held"}"#
    );
    // Socket path is data_dir/keep-at.sock.
    let dir = std::path::Path::new("/tmp/x");
    assert_eq!(
        keep_at::live::socket_path(dir),
        std::path::PathBuf::from("/tmp/x/keep-at.sock")
    );
}

#[test]
fn offline_query_is_none() {
    // No daemon, no socket: query returns None (caller falls back to files).
    let dir = tempfile::tempdir().unwrap().keep();
    assert!(keep_at::live::query(&dir, &keep_at::live::Request::Runtime).is_none());
    assert!(keep_at::live::query(&dir, &keep_at::live::Request::Held).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn live_round_trip_matches_engine() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let fixtures = vec![
        Fixture::new("live-a", 100_000, 3),
        Fixture::new("live-b", 200_000, 3),
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
    let cfg = test_config(data_dir.clone(), storage_dir, test_port(61), 1 << 30);
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
    assert_eq!(engine.held_torrents().len(), 2, "both held");

    // Build a LiveHandle over the Engine's real session + held set (same
    // construction the daemon uses in start_live_server) and serve it on
    // a scratch data dir so the test never touches the engine's own dir.
    let serve_dir = tempfile::tempdir().unwrap().keep();
    let handle = engine.test_live_handle();
    let dir2 = serve_dir.clone();
    let srv = tokio::spawn(async move {
        keep_at::live::serve(dir2, handle).await;
    });
    // Wait for the socket file to appear (bind is async).
    let sock = keep_at::live::socket_path(&serve_dir);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !sock.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "socket never appeared"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Runtime view agrees with the Engine's held set.
    let resp = keep_at::live::query(&serve_dir, &keep_at::live::Request::Runtime)
        .expect("runtime answers");
    match resp {
        keep_at::live::Response::Runtime(v) => {
            assert_eq!(v.held_torrents, 2, "live held matches engine");
            assert_eq!(
                v.seeding_torrents + v.downloading_torrents,
                2,
                "seeding + downloading = held"
            );
            assert!(v.disk_limit_bytes > 0, "limits plumbed through");
        }
        other => panic!("expected runtime, got {other:?}"),
    }

    // Held view lists both torrents with titles + hashes + sizes.
    let resp =
        keep_at::live::query(&serve_dir, &keep_at::live::Request::Held).expect("held answers");
    match resp {
        keep_at::live::Response::Held(view) => {
            assert_eq!(view.torrents.len(), 2, "both torrents listed");
            let titles: Vec<_> = view.torrents.iter().map(|t| t.title.as_str()).collect();
            assert!(titles.contains(&"live-a") && titles.contains(&"live-b"));
            for t in &view.torrents {
                assert!(t.size_bytes == 100_000 || t.size_bytes == 200_000);
                // Fresh adds: not finished yet (the socket reports live
                // session truth, not the on-disk heuristic).
                assert!(!t.finished, "fresh download not misreported as seeding");
            }
        }
        other => panic!("expected held, got {other:?}"),
    }

    srv.abort();
    engine.close().await;
}
