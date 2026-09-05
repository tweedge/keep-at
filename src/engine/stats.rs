//! Runtime stats collection: what the node holds, disk use vs limits,
//! transfer totals, memory. Persisted for `keep-at status`.

use std::path::Path;
use std::time::Instant;

use anyhow::Result;

use crate::engine::storage::dir_size_bytes;
use crate::netstats::{self, RuntimeStats};

pub fn runtime_stats_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("runtime-stats.json")
}

/// Collect a RuntimeStats from live session + state + disk.
pub fn collect(
    api: &librqbit::Api,
    started_at: Instant,
    held: usize,
    seeding: usize,
    disk_used: u64,
    disk_limit: u64,
) -> RuntimeStats {
    let snap = api.api_session_stats();
    let rss = process_rss_bytes();
    RuntimeStats {
        collected_at: Some(chrono::Utc::now()),
        uptime_seconds: started_at.elapsed().as_secs(),
        held_torrents: held,
        seeding_torrents: seeding,
        downloading_torrents: held.saturating_sub(seeding),
        disk_used_bytes: disk_used,
        disk_limit_bytes: disk_limit,
        useful_bytes_uploaded: snap.counters.uploaded_bytes,
        useful_bytes_downloaded: snap.counters.fetched_bytes,
        total_bytes_uploaded: snap.counters.uploaded_bytes,
        total_bytes_downloaded: snap.counters.fetched_bytes,
        active_peers: snap.peers.live as usize,
        process_rss_bytes: rss,
        heap_bytes: 0,
        tasks: 0,
    }
}

/// Sum on-disk bytes + limits across locations.
pub fn disk_usage(locations: &[(std::path::PathBuf, u64)]) -> (u64, u64) {
    let mut used = 0u64;
    let mut limit = 0u64;
    for (path, lim) in locations {
        used = used.saturating_add(dir_size_bytes(path));
        limit = limit.saturating_add(*lim);
    }
    (used, limit)
}

#[cfg(target_os = "linux")]
fn process_rss_bytes() -> u64 {
    // /proc/self/statm: second field is resident pages.
    let data = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let rss_pages: u64 = data
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    rss_pages * 4096
}

#[cfg(not(target_os = "linux"))]
fn process_rss_bytes() -> u64 {
    0
}

pub fn save(data_dir: &Path, s: &RuntimeStats) -> Result<()> {
    let r = netstats::save_runtime(&runtime_stats_path(data_dir), s);
    if let Err(e) = &r {
        tracing::warn!("failed to persist runtime stats: {e:#}");
    }
    r
}
