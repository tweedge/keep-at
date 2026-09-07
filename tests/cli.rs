//! Config/CLI matrix at the user-visible contract level: help text,
//! missing-storage errors, storage-pair mismatch errors, starter-config
//! generation, and status with no daemon. Binary-level via assert_cmd on
//! temp dirs. These survive every refactor because they assert what the
//! user sees, never internals.

use assert_cmd::Command;
use predicates::prelude::*;

fn keep_at() -> Command {
    Command::cargo_bin("keep-at").expect("keep-at binary builds")
}

#[test]
fn help_lists_core_commands() {
    keep_at()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains("network-status"))
        .stdout(predicate::str::contains("hosted-torrents"))
        .stdout(predicate::str::contains("self-update"));
}

#[test]
fn run_help_lists_storage_flags() {
    keep_at()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("storage-location"))
        .stdout(predicate::str::contains("storage-limit"))
        .stdout(predicate::str::contains("aggressiveness"))
        .stdout(predicate::str::contains("max-ram"));
}

#[test]
fn missing_storage_errors_loudly() {
    // No flags, no config, no service config: must fail with a message
    // telling the operator what to pass — never silently run with no space.
    keep_at()
        .args(["run", "--port", "47601"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("storage"));
}

#[test]
fn storage_pair_mismatch_errors() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    // Location without a matching limit.
    keep_at()
        .args([
            "run",
            "--storage-location",
            storage.to_str().unwrap(),
            "--port",
            "47602",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("storage-limit"));
    // Limit without a location is also an error (a bare --storage-limit
    // pairs with --storage or --storage-location, and keep-at will not
    // guess which drive you meant).
    let data = dir.path().join("data");
    keep_at()
        .args([
            "run",
            "--storage-limit",
            "1G",
            "--data-dir",
            data.to_str().unwrap(),
            "--port",
            "47603",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("storage-location"));
}

#[test]
fn starter_config_generated_for_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("sub").join("keep-at.yaml");
    keep_at()
        .args(["run", "--config", cfg.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("starter config"));
    // The starter file now exists with a blank limit field.
    let text = std::fs::read_to_string(&cfg).expect("starter written");
    assert!(text.contains("limit:"), "starter has a limit field");
}

#[test]
fn status_with_no_daemon_reports_not_running() {
    let dir = tempfile::tempdir().unwrap();
    keep_at()
        .args(["status", "--data-dir", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("not running"));
}

#[test]
fn version_prints() {
    keep_at()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("keep-at "));
}
