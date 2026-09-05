//! RAM budget: torrent-count cap derived from system RAM.
//! Ported from internal/engine/ram.go (Go), re-based on measured rqbit costs.
//!
//! Measured on librqbit 9.0.1 (release profile) against real Academic
//! Torrents metainfo:
//!
//! - Paused torrent (handle + metadata + bitfields, no peers): ~126 KiB.
//! - Live torrent, no peers: ~150 KiB fixed + ~32 B/piece of chunk/piece
//!   bookkeeping (slope fit over 64..32768 pieces/torrent: 29.6 B/piece).
//! - Live torrent with peers: each live peer holds a 32 KiB socket read
//!   buffer plus task/stack overhead, so per-peer cost scales with the
//!   session's peer limit — the dominant term for a busy seeder.
//! - AT catalog piece counts are small (median well under 1k pieces), so the
//!   piece term is usually single-digit KiB; the peer term dominates.
//!
//! The model therefore prices a torrent as
//! `BASE + PIECES * piece_count + PEER * peer_limit`, with the peer limit
//! itself scaled down on small-RAM hosts (see peer_limit_for_budget).

use crate::config::SYSTEM_RAM_FRACTION_HARD_CAP;

/// Fixed per-torrent cost: rqbit handle + metadata + bitfield skeleton +
/// session bookkeeping. Measured ~150 KiB live; 256 KiB keeps headroom for
/// allocator fragmentation (system malloc retains freed arenas — observed
/// ~1.9x steady-state in the debug build).
pub const PER_TORRENT_RAM_BASE: u64 = 256 * 1024;

/// Marginal cost per piece: ~32 B measured (chunk-status bit + queue,
/// needed, selected, have bits + piece-tracker reservation entries).
/// 64 B doubles the fit with margin for allocator overhead.
pub const PER_PIECE_RAM: u64 = 64;

/// Per live-peer-connection cost at the given peer limit. Each live peer
/// holds a 32 KiB socket read buffer (rqbit read_buf::BUFLEN) plus task and
/// channel overhead; 48 KiB/peer is the measured-planning figure.
pub const PER_PEER_RAM: u64 = 48 * 1024;

/// Deprecated flat estimate, kept for the hard-cap log line only.
pub fn per_torrent_ram() -> u64 {
    crate::config::PER_TORRENT_RAM_BASE + 12 * 256 * 1024
}

/// RAM footprint of one torrent with `piece_count` pieces at `peer_limit`
/// live peers. Unknown piece counts (0) price as 1 piece — never free.
pub fn torrent_ram(piece_count: u32, peer_limit: usize) -> u64 {
    PER_TORRENT_RAM_BASE
        .saturating_add(PER_PIECE_RAM.saturating_mul(piece_count.max(1) as u64))
        .saturating_add(PER_PEER_RAM.saturating_mul(peer_limit as u64))
}

/// Peer limit scaled to the RAM budget: 20 on comfortable hosts, down to 4
/// on tiny ones. Peer buffers are the dominant per-torrent term, so this is
/// the main lever that lets a 512 MiB node hold 2-3x more torrents — at the
/// cost of slower per-torrent swarms, which is the right trade for a seeder
/// whose torrents are mostly already complete.
pub fn peer_limit_for_budget(budget: u64) -> usize {
    if budget >= 4 * 1024 * 1024 * 1024 {
        20
    } else if budget >= 1024 * 1024 * 1024 {
        12
    } else if budget >= 256 * 1024 * 1024 {
        8
    } else {
        4
    }
}

/// Typical AT torrent for cap math: ~1k pieces at the budget's peer limit.
/// Real candidates price individually via [`torrent_ram`]; this is only the
/// planning figure that turns the byte budget into the torrent-count cap.
pub fn typical_torrent_ram(budget: u64) -> u64 {
    torrent_ram(1024, peer_limit_for_budget(budget))
}

pub fn system_total_ram() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

/// Returns (budget, hard_cap, max_torrents). Unmeasurable RAM (0) => no
/// RAM-driven cap (usize::MAX, or configured max / typical footprint).
pub fn max_torrents_for_budget(system_total: u64, configured_max_ram: u64) -> (u64, u64, usize) {
    if system_total == 0 {
        let max = match configured_max_ram {
            0 => usize::MAX,
            m => (m / typical_torrent_ram(m).max(1)) as usize,
        };
        return (configured_max_ram, 0, max);
    }
    let hard_cap = (system_total as f64 * SYSTEM_RAM_FRACTION_HARD_CAP) as u64;
    let mut budget = hard_cap;
    if configured_max_ram > 0 {
        budget = configured_max_ram.min(hard_cap);
    }
    let max = (budget / typical_torrent_ram(budget).max(1)) as usize;
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
        // 8 GiB budget -> peer limit 20 -> ~1.25 MiB typical -> ~5400 slots.
        assert!(max > 4000 && max < 8000, "max={max}");
        // configured max respected and clamped
        let (_, _, max2) = max_torrents_for_budget(8 * 1024 * 1024 * 1024, 1024 * 1024 * 1024);
        assert!(max2 < max, "max2={max2} max={max}");
        // unmeasurable => uncapped
        let (_, _, max3) = max_torrents_for_budget(0, 0);
        assert_eq!(max3, usize::MAX);
    }

    #[test]
    fn pi_sized_node() {
        // 512 MiB Pi: budget 410 MiB, peer limit 8, typical ~0.7 MiB.
        let (budget, _, max) = max_torrents_for_budget(512 * 1024 * 1024, 0);
        assert_eq!(budget, (512.0 * 1024.0 * 1024.0 * 0.8) as u64);
        assert_eq!(peer_limit_for_budget(budget), 8);
        assert!(max > 400 && max < 900, "max={max}");
    }

    #[test]
    fn footprint_pieces_not_bytes() {
        // A 2 GiB 100-piece torrent costs less RAM than a 2 GiB 100k-piece one.
        let small = torrent_ram(100, 8);
        let big = torrent_ram(100_000, 8);
        assert!(big > small * 5, "small={small} big={big}");
        // Unknown piece count never prices free.
        assert!(torrent_ram(0, 8) >= PER_TORRENT_RAM_BASE);
    }

    #[test]
    fn peer_limit_steps_down() {
        assert_eq!(peer_limit_for_budget(8 * 1024 * 1024 * 1024), 20);
        assert_eq!(peer_limit_for_budget(2 * 1024 * 1024 * 1024), 12);
        assert_eq!(peer_limit_for_budget(410 * 1024 * 1024), 8);
        assert_eq!(peer_limit_for_budget(100 * 1024 * 1024), 4);
    }
}
