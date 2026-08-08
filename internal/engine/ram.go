package engine

import (
	"fmt"

	"golang.org/x/sys/unix"
)

// SystemRAMFractionHardCap is the most of the host's physical memory keep-at
// will ever allow itself to plan around, regardless of what --max-ram says.
// keep-at is meant to run unattended and coexist with whatever else is on the
// box (a Pi's OS, other services, the arr stack, etc.), so it refuses to
// treat more than this share as available even if the operator asks for more.
const SystemRAMFractionHardCap = 0.8

// PerTorrentRAMBase is the fixed per-torrent overhead keep-at pays simply for
// holding a torrent in the client: piece hashes, per-piece completion state,
// hasher goroutines, and the torrent struct itself. It's independent of the
// torrent's byte size or piece count within the ranges Academic Torrents uses
// (the dominant, size-dependent term is the peer-connection buffer pool,
// computed separately from the connection settings).
//
// The number below is deliberately a conservative, round estimate; the goal is
// a safe upper bound for planning how many torrents fit in a RAM budget, not a
// precise accounting of live allocations.
const PerTorrentRAMBase = 1 << 20 // 1 MiB

// PerTorrentConnRAM returns the peak RAM a single held torrent's peer
// connections can buffer, given the client's connection settings. The buffer
// pool is per-torrent in anacrolix/torrent (each torrent keeps up to
// EstablishedConnsPerTorrent independent peer connections, each holding up to
// MaxAllocPeerRequestDataPerConn of upload data), so this is exactly the term
// that scales with how many torrents keep-at holds - which is why capping the
// torrent count is what bounds RAM.
func PerTorrentConnRAM(establishedConnsPerTorrent int, maxAllocPeerRequestDataPerConn int) int64 {
	return int64(establishedConnsPerTorrent) * int64(maxAllocPeerRequestDataPerConn)
}

// SystemTotalRAM returns the host's total physical memory in bytes. It uses
// the portable unix.Sysinfo interface, which covers the Linux, macOS, and
// (via the Go build) Windows targets keep-at ships for. A non-fatal error
// returns 0 so callers can fall back to a configured default rather than
// refusing to start.
func SystemTotalRAM() (int64, error) {
	var info unix.Sysinfo_t
	if err := unix.Sysinfo(&info); err != nil {
		return 0, fmt.Errorf("engine: reading system RAM: %w", err)
	}
	// Unit is MemUnit bytes (usually 1 on modern kernels), not 1024.
	total := int64(info.Totalram) * int64(info.Unit)
	return total, nil
}

// ramBudget computes how much RAM keep-at will let itself plan around, the
// hard 80%-of-system cap, and how many torrents that funds at the given
// per-torrent footprint.
//
// userMaxRAM is the operator's --max-ram setting, or 0 to mean "use the full
// hard cap" (keep-at's simple default: spend up to 80% of system RAM). configMax
// is the configured maximum (also honoured when --max-ram is unset, so a config
// file can pin it). The returned budget is min(userMaxRAM-or-configMax, hard
// cap, full system RAM).
func ramBudget(systemTotal int64, userMaxRAM int64, configMax int64, perTorrent int64) (budget int64, hardCap int64, maxTorrents int) {
	hardCap = int64(float64(systemTotal) * SystemRAMFractionHardCap)
	if systemTotal > 0 && hardCap > systemTotal {
		hardCap = systemTotal
	}

	// Start from the operator's explicit --max-ram, fall back to the
	// config-file max, fall back to the full hard cap.
	budget = hardCap
	switch {
	case userMaxRAM > 0:
		budget = userMaxRAM
	case configMax > 0:
		budget = configMax
	}

	if hardCap > 0 && budget > hardCap {
		budget = hardCap
	}
	if budget < 0 {
		budget = 0
	}

	if perTorrent <= 0 {
		maxTorrents = 0
	} else {
		maxTorrents = int(budget / perTorrent)
	}
	return budget, hardCap, maxTorrents
}
