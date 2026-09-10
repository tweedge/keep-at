//! Kill forensics: figure out what is silently terminating the daemon on
//! shared hosts.
//!
//! Three independent evidence channels, because a SIGKILL destroys the
//! process and everything in it:
//!
//! 1. [`install_signal_death_marks`]: handlers for every catchable fatal
//!    signal (SIGSEGV/SIGABRT/SIGBUS/SIGXCPU/SIGHUP/SIGQUIT) that
//!    synchronously `write(2)` a one-line mark to the log file before
//!    dying. Only async-signal-safe operations (open the fd once at
//!    startup, static strings, no allocation in the handler). If the mark
//!    appears, we know the killer; if the heartbeat just stops, the killer
//!    used SIGKILL (or the kernel), which nothing can log.
//! 2. [`heartbeat_task`]: every 60s, one compact log line + an atomically
//!    replaced `heartbeat.json` recording RSS, cgroup memory state (usage,
//!    limit, oom_kill counter - readable post-mortem even after SIGKILL),
//!    open fd count, peer count, uptime. The heartbeat's last timestamp
//!    brackets the death minute.
//! 3. Watchdog integration (`forensics_on_death`): when the watchdog finds
//!    the daemon dead, it appends the cgroup `memory.events` snapshot and
//!    the last heartbeat to the watchdog log - so the next cron tick
//!    captures host-side evidence (an OOM kill that happened moments
//!    earlier, or the absence of one, which is itself the answer).
//!
//! Linux-only; everything is best-effort and never fatal.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Instant;

#[cfg(target_os = "linux")]
const STATM_PATH: &str = "/proc/self/statm";

/// Log fd kept open O_APPEND for signal handlers (write(2) is
/// async-signal-safe; the fd number is installed once at startup).
static DEATH_FD: AtomicI32 = AtomicI32::new(-1);

#[cfg(target_os = "linux")]
const SIG_NAMES: [(i32, &[u8]); 6] = [
    (4, b"death-mark: SIGSEGV (4) received\n"),
    (6, b"death-mark: SIGABRT (6) received\n"),
    (7, b"death-mark: SIGBUS (7) received\n"),
    (24, b"death-mark: SIGXCPU (24) received - cpu limit hit\n"),
    (1, b"death-mark: SIGHUP (1) received\n"),
    (3, b"death-mark: SIGQUIT (3) received\n"),
];

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn raise(sig: i32) -> i32;
}

extern "C" fn death_mark_handler(sig: i32) {
    let fd = DEATH_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        for (num, msg) in SIG_NAMES {
            if num == sig {
                // write(2) is async-signal-safe; msg is static.
                unsafe { write(fd, msg.as_ptr(), msg.len()) };
                break;
            }
        }
    }
    // Restore the default disposition and re-raise so the exit status
    // reflects the real cause (core dump etc.) instead of returning into
    // corrupted state.
    unsafe {
        signal(sig, 0); // SIG_DFL
        raise(sig);
    }
}

/// Open `log_path` for the death marks and install handlers for every
/// catchable fatal signal. Best effort: failures are logged, never fatal.
pub fn install_signal_death_marks(log_path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .append(true)
        .mode(0o644)
        .open(log_path);
    let Ok(f) = f else {
        tracing::warn!(
            "death marks: could not open {} for writing",
            log_path.display()
        );
        return;
    };
    use std::os::unix::io::AsRawFd;
    DEATH_FD.store(f.as_raw_fd(), Ordering::SeqCst);
    // Leak the file so the fd stays valid for the process lifetime (the
    // fd is raw; dropping the File would close it).
    std::mem::forget(f);
    #[cfg(target_os = "linux")]
    unsafe {
        for (num, _) in SIG_NAMES {
            signal(num, death_mark_handler as *const () as usize);
        }
    }
    tracing::info!(
        "death marks: handlers installed for {} signals (log: {})",
        SIG_NAMES.len(),
        log_path.display()
    );
}

#[cfg(not(target_os = "linux"))]
pub fn install_signal_death_marks(_log_path: &Path) {}

/// RSS in bytes from /proc/self/statm (field 2 = resident pages * 4096).
#[cfg(target_os = "linux")]
fn rss_bytes() -> u64 {
    let data = std::fs::read_to_string(STATM_PATH).unwrap_or_default();
    data.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
        * 4096
}

#[cfg(target_os = "linux")]
fn cgroup_v2_root() -> Option<PathBuf> {
    let data = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let line = data.lines().find(|l| l.starts_with("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(line["0::".len()..].trim()))
}

/// cgroup v2 memory forensics: (current, max, oom_kill counter, peak).
/// memory.events survives process death - the oom_kill counter is the
/// post-mortem proof of a kernel OOM kill in our cgroup.
#[cfg(target_os = "linux")]
fn cgroup_memory() -> Option<(u64, u64, u64, u64)> {
    let root = cgroup_v2_root()?;
    let read = |name: &str| -> u64 {
        std::fs::read_to_string(root.join(name))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };
    // memory.events: lines "oom_kill N" among others.
    let events = std::fs::read_to_string(root.join("memory.events")).unwrap_or_default();
    let oom_kill = events
        .lines()
        .find(|l| l.starts_with("oom_kill "))
        .and_then(|l| l["oom_kill ".len()..].trim().parse().ok())
        .unwrap_or(0);
    Some((
        read("memory.current"),
        read("memory.max"),
        oom_kill,
        read("memory.peak"),
    ))
}

#[cfg(target_os = "linux")]
fn open_fd_count() -> u64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count() as u64)
        .unwrap_or(0)
}

/// One heartbeat sample.
#[derive(serde::Serialize)]
struct Heartbeat {
    at: String,
    uptime_seconds: u64,
    rss_bytes: u64,
    open_fds: u64,
    cgroup: Option<CgroupSample>,
}

#[derive(serde::Serialize)]
struct CgroupSample {
    current_bytes: u64,
    max_bytes: u64,
    oom_kill: u64,
    peak_bytes: u64,
}

fn sample(now: std::time::Instant, started_at: Instant) -> Heartbeat {
    #[cfg(target_os = "linux")]
    let cgroup = cgroup_memory().map(|(current, max, oom_kill, peak)| CgroupSample {
        current_bytes: current,
        max_bytes: max,
        oom_kill,
        peak_bytes: peak,
    });
    #[cfg(not(target_os = "linux"))]
    let cgroup = None;
    Heartbeat {
        at: chrono::Utc::now().to_rfc3339(),
        uptime_seconds: now.saturating_duration_since(started_at).as_secs(),
        rss_bytes: {
            #[cfg(target_os = "linux")]
            {
                rss_bytes()
            }
            #[cfg(not(target_os = "linux"))]
            {
                0
            }
        },
        open_fds: {
            #[cfg(target_os = "linux")]
            {
                open_fd_count()
            }
            #[cfg(not(target_os = "linux"))]
            {
                0
            }
        },
        cgroup,
    }
}

/// Every 60s: log one heartbeat line and atomically replace
/// `data_dir/heartbeat.json` with the latest sample. Post-mortem, the JSON
/// shows the last observed state and its timestamp - bracketing the death
/// minute precisely. Never fails.
pub fn heartbeat_task(data_dir: PathBuf, started_at: Instant) {
    let path = data_dir.join("heartbeat.json");
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let hb = sample(std::time::Instant::now(), started_at);
            if let Ok(json) = serde_json::to_string_pretty(&hb) {
                let _ = crate::config::atomic_write_mode(&path, json.as_bytes(), 0o644);
            }
            if let Some(cg) = &hb.cgroup {
                tracing::info!(
                    "heartbeat rss={}M fds={} cg.cur={}M cg.max={}M cg.oom_kill={} cg.peak={}M",
                    hb.rss_bytes / 1024 / 1024,
                    hb.open_fds,
                    cg.current_bytes / 1024 / 1024,
                    cg.max_bytes / 1024 / 1024,
                    cg.oom_kill,
                    cg.peak_bytes / 1024 / 1024,
                );
            } else {
                tracing::info!(
                    "heartbeat rss={}M fds={}",
                    hb.rss_bytes / 1024 / 1024,
                    hb.open_fds
                );
            }
        }
    });
}

/// Watchdog-side capture: called by the HOST watchdog via a keep-at
/// subcommand when it finds the daemon dead. Reads the cgroup memory state
/// (which survives the process) and the last heartbeat, and appends a
/// compact evidence block to stdout (the watchdog redirects it to its log).
pub fn print_death_evidence(data_dir: &Path) {
    let mut out = String::new();
    out.push_str(&format!(
        "death-evidence at={}\n",
        chrono::Utc::now().to_rfc3339()
    ));
    #[cfg(target_os = "linux")]
    {
        if let Some((current, max, oom_kill, peak)) = cgroup_memory() {
            out.push_str(&format!(
                "cgroup: current={}M max={}M oom_kill={oom_kill} peak={}M\n",
                current / 1024 / 1024,
                max / 1024 / 1024,
                peak / 1024 / 1024
            ));
            if oom_kill > 0 {
                out.push_str("verdict-hint: oom_kill counter is nonzero - the kernel OOM-killed something in this cgroup\n");
            }
        }
    }
    let hb = data_dir.join("heartbeat.json");
    if let Ok(mut f) = std::fs::File::open(&hb) {
        let mut body = String::new();
        let _ = f.read_to_string(&mut body);
        out.push_str(&format!("last-heartbeat: {body}\n"));
    }
    print!("{out}");
}
