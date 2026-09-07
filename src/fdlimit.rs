//! File-descriptor budget: keep-at holds one fd per file of every held
//! torrent (rqbit's FilesystemStorage opens all non-padding files at add
//! time and keeps them open), plus one fd per live peer connection. A
//! many-file candidate added near the RLIMIT_NOFILE ceiling fails with
//! "Too many open files" (os error 24) — a host-capacity refusal, not a
//! candidate defect. This module is the native prevention:
//!
//! - [`raise_soft_limit`]: at startup, raise the soft RLIMIT_NOFILE toward
//!   the hard limit (best effort, logged, never fatal).
//! - [`fd_headroom`]: the admission guard's input — how many more fds the
//!   process can open before hitting the soft ceiling.
//!
//! Linux-only (getrlimit/setrlimit via raw bindings, like the statvfs
//! binding in engine::storage; /proc/self/fd for the open count).

/// Target soft limit: matches the systemd unit's LimitNOFILE, so manual
/// `run` invocations get the same headroom as the service.
pub const TARGET_SOFT_LIMIT: u64 = 65536;

/// Fds reserved for everything that is NOT torrent file handles: peer
/// sockets (peer_limit per torrent, bounded by held count but bursty),
/// listener + DHT + tracker connections, stdio, log file, catalog fetches.
/// The admission guard refuses an add when `headroom < need + RESERVE`, so
/// a many-file add can never starve the sockets the node already holds.
pub const SOCKET_RESERVE: u64 = 512;

/// Current (soft, hard) RLIMIT_NOFILE. None when the query itself fails
/// (non-Linux, seccomp) — callers treat that as "no information", never as
/// "no limit".
pub fn current_limits() -> Option<(u64, u64)> {
    let mut lim = RLimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes a struct rlimit into lim on success.
    let rc = unsafe { getrlimit(RLIMIT_NOFILE, &mut lim) };
    if rc != 0 {
        return None;
    }
    Some((lim.rlim_cur, lim.rlim_max))
}

/// Number of fds this process currently holds, via /proc/self/fd.
pub fn open_count() -> Option<u64> {
    let n = std::fs::read_dir("/proc/self/fd").ok()?.count();
    u64::try_from(n).ok()
}

/// How many more fds the process can open before the soft ceiling:
/// `soft - open`. Saturates at 0; None when either input is unreadable.
pub fn fd_headroom() -> Option<u64> {
    let (soft, _) = current_limits()?;
    let open = open_count()?;
    Some(soft.saturating_sub(open))
}

/// Raise the soft RLIMIT_NOFILE toward [`TARGET_SOFT_LIMIT`], capped by the
/// hard limit. Best effort: logs the outcome, never fails. Called once at
/// startup (run/start foreground and the daemon child alike — the daemon is
/// spawned from this same binary, so it inherits the raised limit).
pub fn raise_soft_limit() {
    match current_limits() {
        Some((soft, hard)) => {
            let target = TARGET_SOFT_LIMIT.min(hard);
            if target <= soft {
                tracing::info!("fd limit: soft {soft} hard {hard} (already at target)");
                return;
            }
            let lim = RLimit {
                rlim_cur: target as RlimT,
                rlim_max: hard as RlimT,
            };
            // SAFETY: setrlimit with a valid pointer; only lowers/raises
            // soft within the existing hard cap, no other process affected.
            let rc = unsafe { setrlimit(RLIMIT_NOFILE, &lim) };
            if rc == 0 {
                tracing::info!("fd limit: raised soft {soft} -> {target} (hard {hard})");
            } else {
                tracing::warn!(
                    "fd limit: could not raise soft {soft} -> {target} (hard {hard}): {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        None => {
            tracing::debug!("fd limit: could not query RLIMIT_NOFILE; skipping raise");
        }
    }
}

/// Pure admission check, factored for tests: true when an add needing
/// `need_files` fds fits within `headroom` while keeping [`SOCKET_RESERVE`]
/// fds back for peer sockets and misc. Unknown headroom (None) admits —
/// the guard only refuses on positive knowledge of exhaustion, never on a
/// failed /proc read.
pub fn fits_in_headroom(need_files: u64, headroom: Option<u64>) -> bool {
    match headroom {
        None => true,
        Some(h) => need_files.saturating_add(SOCKET_RESERVE) <= h,
    }
}

// Minimal rlimit bindings (avoids adding a libc dependency).

type RlimT = u64;

#[repr(C)]
struct RLimit {
    rlim_cur: RlimT,
    rlim_max: RlimT,
}

#[cfg(target_os = "linux")]
const RLIMIT_NOFILE: u32 = 7;

#[cfg(not(target_os = "linux"))]
const RLIMIT_NOFILE: u32 = 7;

unsafe extern "C" {
    fn getrlimit(resource: u32, rlim: *mut RLimit) -> i32;
    fn setrlimit(resource: u32, rlim: *const RLimit) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_math() {
        // Fits with room to spare.
        assert!(fits_in_headroom(100, Some(10_000)));
        // Exactly at the reserve boundary still fits.
        assert!(fits_in_headroom(100, Some(100 + SOCKET_RESERVE)));
        // One short refuses.
        assert!(!fits_in_headroom(100, Some(100 + SOCKET_RESERVE - 1)));
        // Exhausted.
        assert!(!fits_in_headroom(1, Some(0)));
        assert!(!fits_in_headroom(1, Some(SOCKET_RESERVE)));
        // Zero-file add still needs the reserve.
        assert!(fits_in_headroom(0, Some(SOCKET_RESERVE)));
        assert!(!fits_in_headroom(0, Some(SOCKET_RESERVE - 1)));
        // Unknown headroom admits (never refuse on failed reads).
        assert!(fits_in_headroom(u64::MAX, None));
    }

    #[test]
    fn live_limits_sane() {
        // On Linux CI/dev this queries the real rlimit + /proc.
        let limits = current_limits();
        assert!(limits.is_some(), "could not read RLIMIT_NOFILE");
        let (soft, hard) = limits.unwrap();
        assert!(soft > 0 && soft <= hard, "soft={soft} hard={hard}");
        let open = open_count();
        assert!(open.is_some(), "could not count /proc/self/fd");
        // Coherent within a small delta: opening /proc/self/fd itself
        // momentarily holds an extra fd, so the count can read up to a
        // couple higher than the headroom math's implied value.
        let headroom = fd_headroom().expect("headroom readable");
        let implied_open = soft.saturating_sub(headroom);
        let open = open.unwrap();
        assert!(
            implied_open.abs_diff(open) <= 4,
            "implied_open={implied_open} open={open} soft={soft}"
        );
    }
}
