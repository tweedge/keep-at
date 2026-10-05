//! Daemon lifecycle: PID files, foreground detection, container
//! detection, process liveness. Linux-only.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub fn pid_path(data_dir: &Path) -> PathBuf {
    data_dir.join("keep-at.pid")
}

/// Write our PID file (background daemon mode). World-readable: `status`
/// (any user) reads it to find the daemon.
pub fn write_pid(data_dir: &Path) -> Result<()> {
    let pid = std::process::id().to_string();
    crate::config::atomic_write(pid_path(data_dir).as_path(), pid.as_bytes())
        .with_context(|| format!("writing {}", pid_path(data_dir).display()))
}

/// Remove our PID file.
pub fn remove_pid(data_dir: &Path) {
    let _ = std::fs::remove_file(pid_path(data_dir));
}

/// Read the PID file; None when absent/unparseable.
pub fn read_pid(data_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path(data_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether pid is a live keep-at process (via /proc cmdline check: the
/// executable basename must be keep-at).
/// Is this argv0 a keep-at binary?
///
/// ONE predicate for every "is this one of our processes?" question in this
/// module. It used to be spelled two ways - `pid_alive` demanded an exact
/// `keep-at` basename while the /proc scanners accepted any argv0 ending in
/// `keep-at` - so `find_any_daemon` could return `/usr/local/bin/my-keep-at`
/// and the very next `pid_alive` call would call it dead and report the
/// daemon as not running. `ends_with` is the more permissive of the two and
/// keeps a renamed install working consistently; the false-positive risk
/// for PID-reuse detection (a recycled PID landing on some other
/// `*-keep-at` program) is negligible next to the certainty of a
/// self-inconsistent answer.
pub fn is_keep_at_argv0(argv0: &[u8]) -> bool {
    let base = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
    let base = String::from_utf8_lossy(base);
    base.ends_with("keep-at")
}

pub fn pid_alive(pid: u32) -> bool {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if cmdline.is_empty() {
        return false;
    }
    let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
    is_keep_at_argv0(argv0)
}

/// Daemon status: running PID (PID file, verified live), else not running.
pub struct Status {
    pub running: bool,
    pub pid: Option<u32>,
}

pub fn status(data_dir: &Path) -> Status {
    if let Some(pid) = read_pid(data_dir) {
        if pid_alive(pid) {
            return Status {
                running: true,
                pid: Some(pid),
            };
        }
    }
    if let Some(pid) = find_foreground(data_dir) {
        return Status {
            running: true,
            pid: Some(pid),
        };
    }
    Status {
        running: false,
        pid: None,
    }
}

/// Find a foreground `keep-at run/start --data-dir DIR` process by scanning
/// /proc. Returns Some(pid) when found.
pub fn find_foreground(data_dir: &Path) -> Option<u32> {
    find_foreground_in(Path::new("/proc"), &data_dir.to_string_lossy())
}

/// Candidate `keep-at run/start` processes under a proc-like directory,
/// as (pid, argv). The single /proc walk both scanners below share: their
/// argv-parse loops used to be maintained twice.
fn keep_at_daemon_procs(proc_dir: &Path) -> Vec<(u32, Vec<String>)> {
    let mut out = Vec::new();
    // Non-numeric entries (`acpi`, `bus`, `cpuinfo`, ...) are skipped, never
    // fatal - an early `?` here used to abort the whole scan on readdir
    // order, making `status` report "not running" against a live daemon.
    let Ok(procs) = std::fs::read_dir(proc_dir) else {
        return out;
    };
    for entry in procs.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        if cmdline.is_empty() {
            continue;
        }
        let parts: Vec<String> = cmdline
            .split(|&b| b == 0)
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        if parts.len() < 2 || !is_keep_at_argv0(parts[0].as_bytes()) {
            continue;
        }
        if !matches!(parts[1].as_str(), "run" | "start") {
            continue;
        }
        out.push((pid, parts));
    }
    out
}

/// The `--data-dir` / `--config` values in a keep-at argv, in either the
/// `--flag VALUE` or `--flag=VALUE` spelling.
fn data_dir_and_config_from_argv(parts: &[String]) -> (Option<String>, Option<String>) {
    let mut dir_arg: Option<String> = None;
    let mut config_arg: Option<String> = None;
    let mut iter = parts[2..].iter();
    while let Some(a) = iter.next() {
        if a == "--data-dir" {
            dir_arg = iter.next().cloned();
        } else if let Some(v) = a.strip_prefix("--data-dir=") {
            dir_arg = Some(v.to_string());
        } else if a == "--config" {
            config_arg = iter.next().cloned();
        } else if let Some(v) = a.strip_prefix("--config=") {
            config_arg = Some(v.to_string());
        }
    }
    (dir_arg, config_arg)
}

/// Inner scan over a proc-like directory (injected for tests).
fn find_foreground_in(proc_dir: &Path, want: &str) -> Option<u32> {
    for (pid, parts) in keep_at_daemon_procs(proc_dir) {
        // Match --data-dir value when present. A daemon started via
        // `start` runs `run --config <resolved>` (no --data-dir flag), so
        // also read the resolved config's data_dir and compare. A bare
        // run/start with the default dir matches only the default.
        let (dir_arg, config_arg) = data_dir_and_config_from_argv(&parts);
        if let Some(d) = dir_arg {
            if d == want {
                return Some(pid);
            }
            continue;
        }
        if let Some(c) = config_arg {
            // Read-only probe of another process's config path: never write
            // a starter for it (a missing file just means no match).
            if let Ok(cfg) = crate::config::Config::load_readonly(std::path::Path::new(&c)) {
                if cfg.data_dir.to_string_lossy() == want {
                    return Some(pid);
                }
            }
            continue;
        }
        if want == crate::config::default_data_dir().to_string_lossy() {
            return Some(pid);
        }
    }
    None
}

/// Find ANY running keep-at run/start daemon and its data dir, regardless
/// of which data dir the caller resolved. Lets read-only commands
/// (`status`, `hosted-torrents`) and `stop` find a flag-run instance whose
/// data dir is non-default and which has no config file to read.
pub fn find_any_daemon() -> Option<(u32, PathBuf)> {
    find_any_daemon_in(Path::new("/proc"))
}

fn find_any_daemon_in(proc_dir: &Path) -> Option<(u32, PathBuf)> {
    for (pid, parts) in keep_at_daemon_procs(proc_dir) {
        let (dir_arg, config_arg) = data_dir_and_config_from_argv(&parts);
        if let Some(d) = dir_arg {
            return Some((pid, PathBuf::from(d)));
        }
        if let Some(c) = config_arg {
            if let Ok(cfg) = crate::config::Config::load_readonly(std::path::Path::new(&c)) {
                return Some((pid, cfg.data_dir));
            }
            continue;
        }
        return Some((pid, crate::config::default_data_dir()));
    }
    None
}

/// Container detection: /.dockerenv or container= env (podman/docker/systemd-nspawn).
pub fn is_containerized() -> bool {
    if Path::new("/.dockerenv").exists() {
        return true;
    }
    for var in ["container", "KUBERNETES_SERVICE_HOST"] {
        if std::env::var_os(var).is_some() {
            return true;
        }
    }
    false
}

/// `start` spawns a detached child via std::process::Command + setsid and
/// exits.
pub fn setsid_spawn(exe: &Path, args: &[String]) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    // Fully detach: stdin/out/err must not hold the caller's pipes (under a
    // tool harness the child inheriting stdout can wedge or die with it).
    // Daemon output goes to the log via --log-file when set; otherwise it is
    // discarded (status/hosted-torrents read state files, not stdout).
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    // Detach into a new session so the child survives our exit.
    unsafe {
        cmd.pre_exec(|| {
            libc_setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawning detached keep-at")?;
    Ok(child.id())
}

unsafe extern "C" {
    fn setsid() -> i32;
}

fn libc_setsid() {
    unsafe {
        setsid();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_proc(entries: &[(&str, Option<&str>)]) -> PathBuf {
        // entries: (name, cmdline with \0 separators or None for plain files)
        let dir = std::env::temp_dir().join(format!("keep-at-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, cmdline) in entries {
            match cmdline {
                Some(c) => {
                    let p = dir.join(name);
                    std::fs::create_dir_all(&p).unwrap();
                    // c carries literal NUL separators already.
                    std::fs::write(p.join("cmdline"), c.as_bytes()).unwrap();
                }
                None => {
                    std::fs::write(dir.join(name), b"").unwrap();
                }
            }
        }
        dir
    }

    #[test]
    fn foreground_scan_survives_non_numeric_entries() {
        // Regression: an early `?` on the pid parse aborted the whole scan
        // when readdir returned a non-numeric entry first (acpi, cpuinfo,
        // ...), making status report "not running" against a live daemon.
        let want_dir = "/tmp/keep-at-want";
        let raw_cmdline = format!("keep-at\u{0}run\u{0}--data-dir\u{0}{want_dir}\u{0}");
        let dir = fake_proc(&[
            ("acpi", None),
            ("bus", None),
            ("cpuinfo", None),
            ("ioports", None),
            ("1234", Some(raw_cmdline.as_str())),
            ("stat", None),
        ]);
        let found = find_foreground_in(&dir, want_dir);
        assert_eq!(found, Some(1234), "scan must reach the real pid dir");
        // Mismatched data dir: keeps scanning, finds nothing.
        assert_eq!(find_foreground_in(&dir, "/tmp/other"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn foreground_scan_matches_config_flag() {
        let dir = std::env::temp_dir().join(format!("keep-at-procc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Non-numeric first, then a --config daemon. Config loads through
        // the real Config::load; point it at a real readable file.
        let cfg_dir = std::env::temp_dir().join(format!("keep-at-proccfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cfg_dir);
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("c.yaml");
        std::fs::write(
            &cfg_path,
            format!(
                "port: {}\ndata_dir: {}/data\nstorage:\n- path: {}/s\n  limit: 200M\n",
                crate::config::DEFAULT_PORT,
                cfg_dir.display(),
                cfg_dir.display()
            ),
        )
        .unwrap();
        let cmdline = format!(
            "keep-at\u{0}run\u{0}--config\u{0}{}\u{0}",
            cfg_path.display()
        );
        let p = dir.join("777");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cmdline"), cmdline.as_bytes()).unwrap();
        std::fs::write(dir.join("uptime"), b"").unwrap();
        let want = format!("{}/data", cfg_dir.display());
        assert_eq!(find_foreground_in(&dir, &want), Some(777));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&cfg_dir);
    }

    #[test]
    fn any_daemon_scan_finds_flag_run_instance() {
        let dir = std::env::temp_dir().join(format!("keep-at-anyd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cmdline = "keep-at\u{0}run\u{0}--data-dir\u{0}/srv/keep-at/data\u{0}";
        let p = dir.join("4242");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cmdline"), cmdline.as_bytes()).unwrap();
        std::fs::write(dir.join("cpuinfo"), b"").unwrap();
        let found = find_any_daemon_in(&dir);
        assert_eq!(
            found,
            Some((4242, PathBuf::from("/srv/keep-at/data"))),
            "non-default flag-run daemon discoverable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The identity predicate must answer the same way whether it is asked
    /// by `pid_alive` (a bare argv0) or by the /proc scanners (a full
    /// argv). It used to be spelled twice - exact basename for `pid_alive`,
    /// `ends_with` for the scanners - so `find_any_daemon` could hand back a
    /// pid that the immediate `pid_alive` verification then called dead.
    #[test]
    fn identity_predicate_agrees_across_its_callers() {
        for argv0 in [
            "keep-at",
            "/usr/local/bin/keep-at",
            "my-keep-at",
            "/opt/keep-at/keep-at",
        ] {
            assert!(
                is_keep_at_argv0(argv0.as_bytes()),
                "{argv0} must count as one of ours"
            );
        }
        for argv0 in [
            "bash",
            "/usr/bin/keepat",
            "/usr/local/bin/keep-ats",
            "python3",
        ] {
            assert!(
                !is_keep_at_argv0(argv0.as_bytes()),
                "{argv0} must NOT count as one of ours"
            );
        }

        // And the end-to-end shape the callers rely on: whatever the
        // scanner finds, the identity predicate accepts.
        let dir = std::env::temp_dir().join(format!("keep-at-anyd2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cmdline = "my-keep-at\u{0}run\u{0}--data-dir\u{0}/srv/keep-at/data\u{0}";
        let p = dir.join("5555");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cmdline"), cmdline.as_bytes()).unwrap();
        let found = find_any_daemon_in(&dir);
        assert_eq!(found, Some((5555, PathBuf::from("/srv/keep-at/data"))));
        let argv0 = cmdline.split('\u{0}').next().unwrap().as_bytes();
        assert!(
            is_keep_at_argv0(argv0),
            "a process the scanner accepted must also pass the liveness check"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
