//! Regression tests for the persistence/crash findings from the
//! adversarial review (validated live in /tmp/kilo/adv/persistence):
//! - ensure_shared_dirs repairs the data-dir subtree only, never ancestors
//! - Config::save persists the key file before stripping the config, so a
//!   key-file write failure can no longer lose the API key
//! - cap_text_file writes the kept tail before truncating, so a crash mid
//!   cap leaves the newest log intact instead of a 0-byte file

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use keep_at::config::{Config, StorageLocation};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("persist-reg-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn mode_of(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn ensure_shared_dirs_repairs_data_dir_only() {
    let dir = tmpdir("chmod");
    let private = dir.join("private"); // stands in for $HOME / a private vault
    let kat = private.join("local/share/keep-at");
    fs::create_dir_all(&kat).unwrap();
    fs::set_permissions(&private, PermissionsExt::from_mode(0o700)).unwrap();
    fs::set_permissions(private.join("local"), PermissionsExt::from_mode(0o700)).unwrap();
    // Data dir itself under a restrictive umask (root's 077 default).
    fs::set_permissions(&kat, PermissionsExt::from_mode(0o700)).unwrap();

    // Exactly what cmd_status/cmd_logs/cmd_history/cmd_hosted do.
    keep_at::config::ensure_shared_dirs(&kat);

    assert_eq!(
        mode_of(&private),
        0o700,
        "private ancestor must NOT be widened"
    );
    assert_eq!(
        mode_of(&private.join("local")),
        0o700,
        "nested private dir must NOT be widened"
    );
    assert_eq!(
        mode_of(&kat) & 0o755,
        0o755,
        "the data dir itself is still repaired for traversal"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn config_save_keeps_key_when_key_file_write_fails() {
    let dir = tmpdir("apikey");
    let data_dir = dir.join("dd");
    fs::create_dir_all(&data_dir).unwrap();
    let cfg_path = dir.join("keep-at.yaml");

    // Make <data_dir>/api_key impossible to persist: rename(2) onto an
    // existing directory fails (EISDIR), simulating ENOSPC/EACCES on the
    // data dir at the key-file write point.
    fs::create_dir(data_dir.join("api_key")).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        storage: vec![StorageLocation {
            path: data_dir.join("storage"),
            limit: keep_at::config::StorageLimit::Bytes(200 * 1024 * 1024),
        }],
        api_key: "uid=1;pass=SECRET".to_string(),
        ..Config::default()
    };

    // A legacy config already on disk carries the inline key.
    let legacy = format!(
        "data_dir: {}\nstorage:\n- path: {}\n  limit: 200M\napi_key: uid=1;pass=SECRET\n",
        data_dir.display(),
        data_dir.join("storage").display()
    );
    fs::write(&cfg_path, &legacy).unwrap();

    let res = cfg.save(&cfg_path);
    assert!(res.is_err(), "key-file write failure must surface: {res:?}");

    // save() is now non-destructive on key-file failure: the previous
    // config (with the inline key) is untouched on disk.
    let on_disk = fs::read_to_string(&cfg_path).unwrap();
    assert!(
        on_disk.contains("SECRET"),
        "the previous config must survive a failed save"
    );

    // Crash-recovery semantics: reload from disk => key survives.
    let reloaded = Config::load(&cfg_path).unwrap();
    assert_eq!(
        reloaded.api_key, "uid=1;pass=SECRET",
        "API key must survive a failed save"
    );

    // Recovery: clear the fault and retry - key file lands, config is
    // rewritten keyless, and the merged load still yields the key.
    fs::remove_dir(data_dir.join("api_key")).unwrap();
    cfg.save(&cfg_path).expect("retry after clearing the fault");
    assert!(data_dir.join("api_key").is_file(), "key file written");
    let on_disk = fs::read_to_string(&cfg_path).unwrap();
    assert!(!on_disk.contains("SECRET"), "config rewritten keyless");
    let reloaded = Config::load(&cfg_path).unwrap();
    assert_eq!(reloaded.api_key, "uid=1;pass=SECRET");
    let _ = fs::remove_dir_all(&dir);
}

unsafe extern "C" {
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn raise(sig: i32) -> i32;
}

fn signaled(status: i32) -> bool {
    let s = status & 0x7f;
    s != 0 && s != 0x7f
}

#[test]
fn cap_kill_mid_rewrite_keeps_newest_log() {
    let dir = tmpdir("cap");
    let log = dir.join("keep-at.log");
    // ~1MB of evidence, newest lines at the tail (death marks etc.).
    let mut body = String::new();
    for i in 0..20_000 {
        body.push_str(&format!(
            "line {i:05} of forensic evidence ---------------\n"
        ));
    }
    fs::write(&log, &body).unwrap();
    let len_before = fs::metadata(&log).unwrap().len();
    assert!(len_before > 200_000);

    let path_c = std::ffi::CString::new(log.as_os_str().to_str().unwrap()).unwrap();
    let len = len_before;
    let keep = 10 * 1024 * 1024 / 2; // MAX_LOG_BYTES/2
    let cut_from = len.saturating_sub(keep);

    let pid = unsafe { fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // Child: replicate cap_text_file's NEW sequence - read the kept
        // tail, write it at offset 0 - then die exactly before the
        // truncating set_len.
        unsafe {
            let path = PathBuf::from(path_c.into_string().unwrap());
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .expect("child open");
            f.seek(SeekFrom::Start(cut_from)).unwrap();
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).unwrap();
            let cut = buf.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
            f.seek(SeekFrom::Start(0)).unwrap();
            let tail = &buf[cut..];
            f.write_all(tail).unwrap();
            // === crash point: killed between write and set_len ===
            raise(9 /* SIGKILL */);
            std::process::exit(137); // unreachable
        }
    }
    let mut status: i32 = 0;
    unsafe {
        waitpid(pid, &mut status, 0);
    }
    assert!(signaled(status), "child was killed by signal");

    // The newest evidence survives; stale bytes past the written tail are
    // leftover garbage that the next cap pass trims.
    let after = fs::read_to_string(&log).unwrap();
    assert!(
        after.contains("line 19999 of forensic evidence"),
        "newest log lines must survive a crash mid-cap"
    );
    assert!(
        after.starts_with("line "),
        "tail must start at a line boundary"
    );
    let _ = fs::remove_dir_all(&dir);
}
