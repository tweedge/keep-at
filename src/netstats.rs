//! Scan progress snapshots and runtime summaries. `network-stats.json`
//! holds cross-boot scan state (progress + p10 seeder floor the next scan
//! needs); `runtime-stats.json` is the offline fallback `status` reads when
//! no daemon is running (the live socket serves instantaneous numbers when
//! one is).

use chrono::{DateTime, Utc};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub scan_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scan_completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub total_candidates: u64,
    #[serde(default)]
    pub processed_candidates: u64,
    #[serde(default)]
    pub seeder_floor: u32,
}

impl Snapshot {
    pub fn in_progress(&self) -> bool {
        self.scan_started_at.is_some() && self.scan_completed_at.is_none()
    }

    pub fn progress_percent(&self) -> f64 {
        if self.total_candidates == 0 {
            return 0.0;
        }
        let pct = self.processed_candidates as f64 / self.total_candidates as f64 * 100.0;
        pct.min(100.0)
    }
}

pub fn load_snapshot(path: &Path) -> Result<Snapshot> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Snapshot::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        Ok(data) => {
            serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))
        }
    }
}

pub fn save_snapshot(path: &Path, s: &Snapshot) -> Result<()> {
    let data = serde_json::to_string_pretty(s).context("marshalling snapshot")?;
    crate::config::atomic_write(path, data.as_bytes())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeStats {
    #[serde(default)]
    pub collected_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub uptime_seconds: u64,
    #[serde(default)]
    pub held_torrents: usize,
    #[serde(default)]
    pub seeding_torrents: usize,
    #[serde(default)]
    pub downloading_torrents: usize,
    #[serde(default)]
    pub disk_used_bytes: u64,
    #[serde(default)]
    pub disk_limit_bytes: u64,
    #[serde(default)]
    pub disk_committed_bytes: u64,
    #[serde(default)]
    pub useful_bytes_uploaded: u64,
    #[serde(default)]
    pub useful_bytes_downloaded: u64,
    #[serde(default)]
    pub total_bytes_uploaded: u64,
    #[serde(default)]
    pub total_bytes_downloaded: u64,
    #[serde(default)]
    pub active_peers: usize,
    #[serde(default)]
    pub process_rss_bytes: u64,
    #[serde(default)]
    pub heap_bytes: u64,
    #[serde(default)]
    pub tasks: usize,
}

impl RuntimeStats {
    pub fn uptime(&self) -> Duration {
        Duration::from_secs(self.uptime_seconds)
    }

    pub fn upload_bits_per_sec(&self) -> f64 {
        if self.uptime_seconds == 0 {
            return 0.0;
        }
        self.total_bytes_uploaded as f64 * 8.0 / self.uptime_seconds as f64
    }

    pub fn download_bits_per_sec(&self) -> f64 {
        if self.uptime_seconds == 0 {
            return 0.0;
        }
        self.total_bytes_downloaded as f64 * 8.0 / self.uptime_seconds as f64
    }

    pub fn disk_used_pct(&self) -> f64 {
        if self.disk_limit_bytes == 0 {
            return 0.0;
        }
        (self.disk_used_bytes as f64 / self.disk_limit_bytes as f64 * 100.0).min(100.0)
    }
}

pub fn load_runtime(path: &Path) -> Result<RuntimeStats> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RuntimeStats::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        Ok(data) => {
            serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))
        }
    }
}

pub fn save_runtime(path: &Path, s: &RuntimeStats) -> Result<()> {
    let data = serde_json::to_string_pretty(s).context("marshalling runtime stats")?;
    crate::config::atomic_write(path, data.as_bytes())
}

/// Accumulates keep-at peer observations during one scan/census.
#[derive(Debug, Default)]
pub struct Tracker {
    nodes: std::collections::HashSet<String>,
    pub seeding_bytes: u64,
    pub leeching_bytes: u64,
}

impl Tracker {
    pub fn observe(&mut self, node_key: String, torrent_size: u64, complete: bool) {
        self.nodes.insert(node_key);
        if complete {
            self.seeding_bytes += torrent_size;
        } else {
            self.leeching_bytes += torrent_size;
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_progress() {
        let s = Snapshot {
            total_candidates: 4,
            processed_candidates: 1,
            ..Default::default()
        };
        assert_eq!(s.progress_percent(), 25.0);
        assert!(!s.in_progress());
    }
}
