//! Process allocator configuration: jemalloc on Linux gnu targets, system
//! allocator elsewhere.
//!
//! The `#[global_allocator]` static itself lives in the BINARY crate
//! (src/bin/keep-at.rs): a library-level allocator would apply to every
//! crate linking keep_at (test harnesses, CLI subcommand processes) and
//! would compile-error any consumer that wants its own allocator. This
//! module only configures it.
//!
//! Why jemalloc: glibc malloc retains freed pages in per-thread arena
//! segments (it only ever returns top-of-heap memory to the OS), so
//! resident memory is the sum of every arena's historical high-water mark
//! and only ratchets upward as scan/check/peer bursts warm more arenas -
//! observed on a 72-core host as 165 arenas x 64 MB = 10.5 GiB retained
//! with zero live peers. jemalloc returns freed pages to the OS
//! continuously: dirty pages decay back over ~10s by default (sigmoidal
//! curve), purged proactively by a background thread. There is no
//! accumulation window and no trim call; RSS tracks the live working set.
//!
//! Background purging is enabled here at runtime (the mallctl route the
//! jemalloc manual recommends) and verified by read-back: without it,
//! lazy-only purging lets freed memory linger ~25s instead of ~10s.
//! Failure to enable is a warning, never fatal - lazy decay still returns
//! memory on allocation activity.
//!
//! Runtime tuning stays possible without code changes via the MALLOC_CONF
//! environment variable (processed after jemalloc's compiled-in defaults)
//! for decay times, arena counts, and everything else - except
//! background_thread, which this module force-enables at startup.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod imp {
    /// Configure the process allocator. Call once at startup, after logging
    /// is initialized so the result is observable in the log. Never fatal.
    pub fn init() {
        match tikv_jemalloc_ctl::background_thread::update(true) {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("allocator: could not enable jemalloc background purging: {e}")
            }
        }
        let purging = tikv_jemalloc_ctl::background_thread::read().unwrap_or(false);
        // SAFETY: raw mallctl read of a documented option name; the value
        // type of opt.dirty_decay_ms is ssize_t (isize).
        let decay = unsafe { tikv_jemalloc_ctl::raw::read::<isize>(b"opt.dirty_decay_ms\0") };
        match (purging, decay) {
            (true, Ok(ms)) => tracing::info!("allocator: jemalloc (background purging on, dirty decay {ms}ms)"),
            (true, Err(_)) => tracing::info!("allocator: jemalloc (background purging on)"),
            (false, _) => tracing::warn!("allocator: jemalloc background purging NOT running - freed pages return to the OS only lazily"),
        }
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod imp {
    /// No-op on non-glibc targets: the system allocator is used as-is.
    pub fn init() {}
}

pub use imp::init;
