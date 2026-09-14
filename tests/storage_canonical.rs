//! Regression tests for storage-location identity (canonicalization):
//! symlink aliases of one directory used to double-budget the same
//! physical location, orphan state.json entries on spelling changes, and
//! double-resolve `limit: max`. Locations are now canonicalized in
//! resolve_all_limits and deduped on the canonical path; legacy state
//! entries are re-keyed at boot.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{test_options, with_timeout, Fixture, Stub, StubState};
use keep_at::config::{Config, StorageLimit, StorageLocation};
use keep_at::state::Torrent;

fn alias_config(data_dir: PathBuf, real: &Path, alias: &Path) -> Config {
    Config {
        data_dir,
        port: 47999,
        storage: vec![
            StorageLocation {
                path: real.to_path_buf(),
                limit: StorageLimit::Bytes(110_000_000),
            },
            StorageLocation {
                path: alias.to_path_buf(),
                limit: StorageLimit::Bytes(110_000_000),
            },
        ],
        scan: keep_at::config::ScanConfig {
            interval: std::time::Duration::from_secs(1),
            rate_limit_per_second: 1000.0,
            moderation_delay: std::time::Duration::ZERO,
            ..Default::default()
        },
        aggressiveness: 0.999999,
        ..Config::default()
    }
}

fn symlink_alias(real: &Path) -> PathBuf {
    #[cfg(unix)]
    std::os::unix::fs::symlink(real, real.with_extension("alias")).unwrap();
    real.with_extension("alias")
}

#[test]
fn validate_rejects_symlink_alias_of_same_directory() {
    let root = tempfile::tempdir().unwrap().keep();
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let alias = symlink_alias(&store);

    // Same raw string twice: the guard fires (it exists).
    let dup = alias_config(tempfile::tempdir().unwrap().keep(), &store, &store);
    assert!(dup.validate().is_err(), "identical paths are rejected");
    // Symlink alias: SAME directory, LEXICALLY different path - the
    // canonical-key dedup must catch it too.
    let cfg = alias_config(tempfile::tempdir().unwrap().keep(), &store, &alias);
    assert!(
        cfg.validate().is_err(),
        "symlink alias of the same directory must be rejected by validate()"
    );
}

#[test]
fn resolve_all_limits_rejects_alias_pair_instead_of_double_budgeting() {
    let root = tempfile::tempdir().unwrap().keep();
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let alias = symlink_alias(&store);
    let cfg = Config {
        data_dir: tempfile::tempdir().unwrap().keep(),
        storage: vec![
            StorageLocation {
                path: store.clone(),
                limit: StorageLimit::All,
            },
            StorageLocation {
                path: alias,
                limit: StorageLimit::All,
            },
        ],
        ..Config::default()
    };
    // Both aliases used to resolve to 0.975x the SAME device - a combined
    // budget of ~1.95x the device. The alias pair must be rejected.
    let resolved = keep_at::engine::resolve_all_limits(&cfg);
    assert!(
        resolved.is_err(),
        "alias pair with limit: max must be rejected, not double-budgeted"
    );
}

#[test]
fn rekey_storage_locations_migrates_legacy_spellings() {
    let root = tempfile::tempdir().unwrap().keep();
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let alias = symlink_alias(&store);

    let st_path = root.join("state.json");
    let mut st = keep_at::state::State::load(&st_path).unwrap();
    // Legacy state entry recorded under the alias spelling (the old config
    // pointed at the symlink); the new config canonicalizes to the real dir.
    st.put(Torrent {
        info_hash: "aa".to_string(),
        title: "t".to_string(),
        size_bytes: 60_000_000,
        storage_location: alias.clone(),
        added_at: chrono::Utc::now(),
        piece_count: 1,
        last_known_seeders: 5,
        completed_pieces: 0,
        last_progress_at: None,
    })
    .unwrap();
    // Historical spelling is invisible to the canonical location's budget...
    assert_eq!(st.bytes_used(&store), 0);
    assert_eq!(st.bytes_used(&alias), 60_000_000);

    // Re-key: the entry's dir canonicalizes into the configured location.
    let canon = store.canonicalize().unwrap();
    assert!(st.rekey_storage_locations(std::slice::from_ref(&canon)));
    assert_eq!(
        st.bytes_used(&canon),
        60_000_000,
        "re-keyed entry counts against the configured location"
    );
    // Idempotent: a second pass changes nothing.
    assert!(!st.rekey_storage_locations(&[canon]));
}

/// Engine-level: an alias pair must fail validation instead of granting two
/// independent 110 MB budgets to one physical directory.
#[tokio::test(flavor = "multi_thread")]
async fn alias_locations_are_rejected_at_engine_construction() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let root = tempfile::tempdir().unwrap().keep();
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let alias = symlink_alias(&store);

    let fixtures = vec![
        Fixture::new("alias-first-60m", 60_000_000, 1),
        Fixture::new("alias-second-60m", 60_000_000, 1),
    ];
    let state2 = Arc::new(std::sync::Mutex::new(StubState::default()));
    let stub = Stub::start(Stub::catalog_xml(&[]), state2.clone());
    let tracker = stub.tracker_url();
    let mut rows2 = Vec::new();
    {
        let mut st = state2.lock().unwrap();
        for f in &fixtures {
            let (hex, _) = st.add(f, &tracker);
            rows2.push((f.title.clone(), hex, f.size));
        }
    }
    let (catalog_base, _srv) = common::serve_catalog(Stub::catalog_xml(&rows2));
    std::mem::forget(_srv);

    let cfg = alias_config(tempfile::tempdir().unwrap().keep(), &store, &alias);
    let built = with_timeout(
        60,
        "engine new",
        keep_at::engine::Engine::new_with_options(cfg, test_options(&catalog_base, &stub.base_url)),
    )
    .await;
    assert!(
        built.is_err(),
        "alias pair must be rejected at engine construction (pre-fix: two budgets admitted 120 MB into a 110 MB directory)"
    );
}
