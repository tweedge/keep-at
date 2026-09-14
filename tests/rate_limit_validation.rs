//! Regression: census rate-limit flags used to bypass all validation
//! (NaN passed through to the probe limiter and panicked it; non-positive
//! silently disabled AT politeness). resolve_census now rejects both.

use keep_at::cli::{CommonArgs, NetworkStatusArgs};

fn census_args(cfg_path: std::path::PathBuf, rate: Option<f64>) -> NetworkStatusArgs {
    NetworkStatusArgs {
        common: CommonArgs {
            config: Some(cfg_path),
            data_dir: None,
        },
        probe_timeout: None,
        api_key: None,
        rate_limit: rate,
    }
}

fn cfg_file(dir: &std::path::Path) -> std::path::PathBuf {
    let p = dir.join("cfg.yaml");
    std::fs::write(
        &p,
        format!(
            "port: 39999\ndata_dir: {}\nstorage:\n- path: {}/s\n  limit: 500G\n",
            dir.display(),
            dir.display()
        ),
    )
    .unwrap();
    p
}

#[test]
fn census_rate_limit_flags_are_validated() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_path = cfg_file(tmp.path());

    for bad in [f64::NAN, -1.0, 0.0] {
        let a = census_args(cfg_path.clone(), Some(bad));
        assert!(
            keep_at::cli::resolve_census(&a).is_err(),
            "rate limit {bad} must be rejected"
        );
    }

    let a_ok = census_args(cfg_path, Some(2.0));
    let cfg = keep_at::cli::resolve_census(&a_ok).expect("positive rate limit accepted");
    assert_eq!(cfg.scan.rate_limit_per_second, 2.0);
}

#[test]
fn daemon_validate_rejects_nan_rate_limit() {
    let mut cfg = keep_at::config::Config {
        storage: vec![keep_at::config::StorageLocation {
            path: "/tmp/keep-at-nan-check".into(),
            limit: keep_at::config::StorageLimit::Bytes(1 << 30),
        }],
        ..keep_at::config::Config::default()
    };
    let scan: keep_at::config::ScanConfig =
        serde_yaml::from_str("rate_limit_per_second: .nan").unwrap();
    assert!(scan.rate_limit_per_second.is_nan());
    cfg.scan = scan;
    assert!(
        cfg.validate().is_err(),
        "NaN rate_limit_per_second must fail validation"
    );
}
