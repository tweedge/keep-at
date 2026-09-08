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
fn run_help_groups_flags_by_purpose() {
    // Help is organized by purpose (Storage, Network, Selection, Limits,
    // Logging), not alphabetically: Storage comes before Network, and the
    // per-group headers exist exactly once each.
    let out = keep_at()
        .args(["run", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out).into_owned();
    for heading in ["Storage:", "Network:", "Selection:", "Limits:", "Logging:"] {
        assert_eq!(
            text.matches(heading).count(),
            1,
            "help has exactly one {heading} group"
        );
    }
    let storage = text.find("Storage:").unwrap();
    let network = text.find("Network:").unwrap();
    let selection = text.find("Selection:").unwrap();
    let limits = text.find("Limits:").unwrap();
    assert!(
        storage < network && network < selection && selection < limits,
        "groups ordered Storage, Network, Selection, Limits"
    );
    // Spot-check membership: the storage pair lives under Storage.
    let storage_section = &text[storage..network];
    assert!(storage_section.contains("--storage-limit"));
    assert!(storage_section.contains("--storage-location"));
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
}

#[test]
fn storage_shorthand_and_bare_limit_resolve() {
    // --storage PATH pairs with one --storage-limit (previously a hard
    // error); a bare --storage-limit fills the default location (also
    // previously a hard error). Both must get PAST storage validation —
    // assert the failure, if any, is never about storage pairing. (The
    // commands proceed to engine startup, which needs no network to fail
    // storage validation first; use a bogus tiny limit to force the
    // storage error path deterministically... instead use valid limits and
    // assert absence of the pairing errors.)
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("s1");
    let data = dir.path().join("d1");
    // --storage shorthand: must NOT complain about pairing. It WILL fail
    // later (tiny limit rejected) or proceed — either way the pairing
    // error is gone. Use a rejected-tiny limit to keep it hermetic-fast:
    // the failure must be the 100M floor, not the pairing.
    keep_at()
        .args([
            "run",
            "--storage",
            storage.to_str().unwrap(),
            "--storage-limit",
            "50M",
            "--data-dir",
            data.to_str().unwrap(),
            "--port",
            "47604",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("under 100M"))
        .stderr(predicate::str::contains("matching").not());
    // Bare --storage-limit: same — pairing error gone, floor error present.
    let data2 = dir.path().join("d2");
    keep_at()
        .args([
            "run",
            "--storage-limit",
            "50M",
            "--data-dir",
            data2.to_str().unwrap(),
            "--port",
            "47605",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("under 100M"))
        .stderr(predicate::str::contains("matching").not());
}

#[test]
fn byte_size_floors_reject_and_hint_max() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    // Storage under 100M rejected, with a max hint.
    keep_at()
        .args([
            "run",
            "--storage-limit",
            "50M",
            "--data-dir",
            data.to_str().unwrap(),
            "--port",
            "47606",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("under 100M"))
        .stderr(predicate::str::contains("max"));
    // Unknown suffix names max too (the reported confusing message).
    keep_at()
        .args([
            "run",
            "--storage-limit",
            "500X",
            "--data-dir",
            data.to_str().unwrap(),
            "--port",
            "47607",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("max"));
    // Bandwidth under 100K rejected.
    keep_at()
        .args([
            "run",
            "--storage-limit",
            "500G",
            "--upload-rate-limit",
            "50",
            "--data-dir",
            data.to_str().unwrap(),
            "--port",
            "47608",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("under 100K"));
}

#[test]
fn max_limit_keyword_accepted() {
    // --storage-limit max resolves against the real filesystem (tmpfs here):
    // must get PAST storage validation. It proceeds to engine startup
    // (catalog fetch), which takes seconds — instead assert via a config
    // file round-trip... simplest hermetic check: the pairing/parse stage
    // accepts it, i.e. failure (if any) is not a parse error. Use --help
    // adjacent: validate() runs inside resolve(); a parse rejection would
    // print "unknown byte-size" or similar. Give it a data dir and a port
    // and check stderr lacks parse/limit errors. (Engine startup may fail
    // later on network — that comes after validation, out of scope.)
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    // Unit-level coverage for max/all parsing lives in config tests; here
    // assert the CLI layer passes the keyword through: stale-cache-free
    // hermetic check via a tiny timeout — the command must not fail fast
    // with a storage error. resolve() happens before any network, so a
    // storage complaint appears instantly; anything else (hang/startup)
    // proves parsing passed. Kill after 3s: absence of instant storage
    // error IS the assertion.
    let mut cmd = keep_at();
    let assert = cmd
        .args([
            "run",
            "--storage",
            dir.path().join("s").to_str().unwrap(),
            "--storage-limit",
            "max",
            "--data-dir",
            data.to_str().unwrap(),
            "--port",
            "47609",
        ])
        .timeout(std::time::Duration::from_secs(3))
        .assert();
    // Either it started (killed by timeout -> failure with empty/storage-free
    // stderr) or it failed on something non-storage. Assert no storage error.
    let out = assert.get_output();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("storage location")
            && !stderr.contains("unknown byte-size")
            && !stderr.contains("matching"),
        "max keyword passes CLI parsing: {stderr}"
    );
}

#[test]
fn starter_config_generated_for_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("sub").join("keep-at.yaml");
    // Guard against the observed CI flake where a pre-existing starter at
    // the fresh tempdir path made the run report "parsing config ... empty
    // byte size" instead of writing one. Fail HERE (with the file present)
    // so a recurrence points at a concurrent writer, not at stderr wording.
    assert!(!cfg.exists(), "config path must start empty");
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
