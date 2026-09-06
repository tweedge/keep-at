//! Daemon lifecycle: PID files, foreground detection, container
//! detection, process liveness. Linux-only.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub fn pid_path(data_dir: &Path) -> PathBuf {
    data_dir.join("keep-at.pid")
}

/// Write our PID file (background daemon mode).
pub fn write_pid(data_dir: &Path) -> Result<()> {
    let pid = std::process::id().to_string();
    std::fs::write(pid_path(data_dir), format!("{pid}\n"))
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
pub fn pid_alive(pid: u32) -> bool {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if cmdline.is_empty() {
        return false;
    }
    let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
    let base = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
    base == b"keep-at"
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
        return Status { running: true, pid };
    }
    Status {
        running: false,
        pid: None,
    }
}

/// Find a foreground `keep-at run/start --data-dir DIR` process by scanning
/// /proc (same lookup Go's status used). Returns Some(pid) when found.
pub fn find_foreground(data_dir: &Path) -> Option<Option<u32>> {
    let want = data_dir.to_string_lossy().into_owned();
    let procs = std::fs::read_dir("/proc").ok()?;
    for entry in procs.flatten() {
        let name = entry.file_name();
        let pid: u32 = name.to_string_lossy().parse().ok()?;
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
        if parts.len() < 2 || !parts[0].ends_with("keep-at") {
            continue;
        }
        if !matches!(parts[1].as_str(), "run" | "start") {
            continue;
        }
        // Match --data-dir value when present. A daemon started via
        // `start` runs `run --config <resolved>` (no --data-dir flag), so
        // also read the resolved config's data_dir and compare. A bare
        // run/start with the default dir matches only the default.
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
        if let Some(d) = dir_arg {
            if d == want {
                return Some(Some(pid));
            }
            continue;
        }
        if let Some(c) = config_arg {
            if let Ok(cfg) = crate::config::Config::load(std::path::Path::new(&c)) {
                if cfg.data_dir.to_string_lossy() == want {
                    return Some(Some(pid));
                }
            }
            continue;
        }
        if want == crate::config::default_data_dir().to_string_lossy() {
            return Some(Some(pid));
        }
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

/// Double-fork detach is unnecessary in Rust: `start` spawns a detached
/// child via std::process::Command + setsid and exits. This helper reports
/// whether stdin/stdout look like a terminal (for log routing parity).
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
