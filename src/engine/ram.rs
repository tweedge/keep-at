//! RAM budget: torrent-count cap derived from system RAM.
//! Ported from internal/engine/ram.go (Go).

use crate::config::SYSTEM_RAM_FRACTION_HARD_CAP;

/// Fixed per-torrent RAM overhead: rqbit torrent bookkeeping plus bounded
/// peer-connection buffers. Conservative planning bound (1 MiB base, same
/// role as Go's PerTorrentRAMBase, plus 12 connections x 256 KiB buffers
/// matching the Go client's per-torrent connection tuning).
pub fn per_torrent_ram() -> u64 {
    crate::config::PER_TORRENT_RAM_BASE + 12 * 256 * 1024
}

pub fn system_total_ram() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

/// Returns (budget, hard_cap, max_torrents). Unmeasurable RAM (0) => no
/// RAM-driven cap (usize::MAX, or configured max / footprint).
pub fn max_torrents_for_budget(system_total: u64, configured_max_ram: u64) -> (u64, u64, usize) {
    if system_total == 0 {
        let max = match configured_max_ram {
            0 => usize::MAX,
            m => (m / per_torrent_ram().max(1)) as usize,
        };
        return (configured_max_ram, 0, max);
    }
    let hard_cap = (system_total as f64 * SYSTEM_RAM_FRACTION_HARD_CAP) as u64;
    let mut budget = hard_cap;
    if configured_max_ram > 0 {
        budget = configured_max_ram.min(hard_cap);
    }
    let max = (budget / per_torrent_ram().max(1)) as usize;
    (budget, hard_cap, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_math() {
        let (budget, hard, max) = max_torrents_for_budget(8 * 1024 * 1024 * 1024, 0);
        assert_eq!(hard, (8.0 * 1024.0 * 1024.0 * 1024.0 * 0.8) as u64);
        assert_eq!(budget, hard);
        assert_eq!(max, (hard / per_torrent_ram()) as usize);
        // configured max respected and clamped
        let (_, _, max2) = max_torrents_for_budget(8 * 1024 * 1024 * 1024, 1024 * 1024 * 1024);
        assert_eq!(max2, (1024 * 1024 * 1024 / per_torrent_ram()) as usize);
        // unmeasurable => uncapped
        let (_, _, max3) = max_torrents_for_budget(0, 0);
        assert_eq!(max3, usize::MAX);
    }
}
