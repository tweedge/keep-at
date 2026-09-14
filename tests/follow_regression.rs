//! Regression tests for the follow loops (`logs`, `history`): they must use
//! tail -F semantics on shrinkage. The 0.8.24 log cap rewrites keep-at.log
//! in place, and history.jsonl rotates at its size cap - before the fix,
//! a follower's stale byte offset stalled it until the file regrew past the
//! old length, then skipped everything written before that offset.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Spawn the real CLI binary with piped stdout; returns the child plus a
/// shared buffer holding everything printed so far (child may keep running).
fn spawn_child(args: &[&str]) -> (std::process::Child, Arc<Mutex<String>>) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_keep-at"));
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn keep-at");
    let out = child.stdout.take().expect("piped stdout");
    let buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let sink = buf.clone();
    std::thread::spawn(move || {
        let mut r = out;
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&chunk[..n])),
            }
        }
    });
    (child, buf)
}

#[test]
fn logs_follow_resumes_from_top_after_truncation() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let log = dir.join("keep-at.log");
    let mut body = String::new();
    for i in 0..40 {
        body.push_str(&format!(
            "L{i:02} --------------------------------------------------\n"
        ));
    }
    std::fs::write(&log, &body).unwrap();

    let cfg = dir.join("config.yaml");
    std::fs::write(
        &cfg,
        format!(
            "data_dir: {}\nlog_file: {}\nstorage:\n- path: {}\n  limit: 1G\n",
            dir.display(),
            log.display(),
            dir.join("storage").display()
        ),
    )
    .unwrap();

    let (mut child, buf) =
        spawn_child(&["logs", "--config", cfg.to_str().unwrap(), "--lines", "1"]);
    std::thread::sleep(Duration::from_millis(1500)); // tail printed; pos = old_len

    // A log-cap pass truncates in place to the newest half (much shorter
    // than the follower's offset), then the daemon writes fresh lines.
    let mut new_content = String::from("NEW-BOOT line of the fresh daemon\n");
    while new_content.len() < 900 {
        new_content.push_str("filler ------------------------------------------------\n");
    }
    std::fs::write(&log, &new_content).unwrap();
    std::thread::sleep(Duration::from_millis(1200));

    std::fs::write(
        &log,
        format!("{new_content}LATE-MARKER fresh content past the reset\n"),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let out = buf.lock().unwrap().clone();
    assert!(
        out.contains("NEW-BOOT"),
        "the follower must restart from the top of the truncated log: {out:?}"
    );
    assert!(
        out.contains("LATE-MARKER"),
        "the follower must keep streaming after the reset: {out:?}"
    );
    child.kill().unwrap();
}

#[test]
fn history_follow_resumes_on_the_new_generation_after_rotation() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = keep_at::history::history_path(&dir);

    // Old generation: one valid event + one long unparseable line, so the
    // follow's starting pos lands ~2100 bytes in.
    let ev = keep_at::history::add_event(
        "0123456789abcdef0123456789abcdef01234567",
        "old event",
        1,
        1,
        Path::new("/mnt/d"),
        keep_at::history::Cause::Fill,
        1.0,
        0.5,
        1,
        "seed-scarcity roll succeeded",
        Vec::new(),
    );
    let mut old = serde_json::to_string(&ev).unwrap();
    old.push('\n');
    let mut junk = String::from("{\"torn\":");
    while junk.len() < 2000 {
        junk.push('x');
    }
    junk.push('\n');
    std::fs::write(&path, format!("{old}{junk}")).unwrap();

    let (mut child, buf) = spawn_child(&[
        "history",
        "--data-dir",
        dir.to_str().unwrap(),
        "--lines",
        "10",
    ]);
    std::thread::sleep(Duration::from_millis(1500)); // tail printed; pos ~= 2100

    // Rotation (exactly what Writer::rotate does): rename to .1, new file.
    std::fs::rename(&path, dir.join("history.jsonl.1")).unwrap();
    let ev_early = keep_at::history::add_event(
        "fedcba9876543210fedcba9876543210fedcba98",
        "SWAP-EARLY after rotation",
        1,
        1,
        Path::new("/mnt/d"),
        keep_at::history::Cause::Swap,
        1.0,
        0.5,
        1,
        "swap",
        Vec::new(),
    );
    let ev_late = keep_at::history::add_event(
        "aaaa0000aaaa0000aaaa0000aaaa0000aaaa0000",
        "ADD-LATE after rotation",
        1,
        1,
        Path::new("/mnt/d"),
        keep_at::history::Cause::Fill,
        1.0,
        0.5,
        1,
        "fill",
        Vec::new(),
    );
    let mut early = serde_json::to_string(&ev_early).unwrap();
    early.push('\n');
    let mut filler = String::new();
    while (early.len() + filler.len()) < 2500 {
        filler.push_str("{\"x\":\"pad----------------------------------------\"}\n");
    }
    let mut late = serde_json::to_string(&ev_late).unwrap();
    late.push('\n');
    std::fs::write(&path, format!("{early}{filler}{late}")).unwrap();

    std::thread::sleep(Duration::from_millis(2500));
    let out = buf.lock().unwrap().clone();

    assert!(
        out.contains("SWAP-EARLY"),
        "events written right after rotation must be shown: {out:?}"
    );
    assert!(
        out.contains("ADD-LATE"),
        "the follower must keep streaming the new generation: {out:?}"
    );
    child.kill().unwrap();
}
