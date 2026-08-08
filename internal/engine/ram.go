package engine

import "math"

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

// ramBudget computes how much RAM keep-at will let itself plan around, the
// hard 80%-of-system cap, and how many torrents that funds at the given
// per-torrent footprint.
//
// systemTotal is the host's physical RAM (see SystemTotalRAM); a value of 0
// means it couldn't be measured on this platform, in which case keep-at
// applies NO RAM-driven torrent cap (it just won't bound by RAM) rather than
// capping to zero. userMaxRAM is the operator's --max-ram setting, or 0 to
// mean "use the full hard cap" (keep-at's simple default: spend up to 80% of
// system RAM). configMax is the configured maximum (also honoured when
// --max-ram is unset, so a config file can pin it). The returned budget is
// min(userMaxRAM-or-configMax, hard cap, full system RAM).
func ramBudget(systemTotal int64, userMaxRAM int64, configMax int64, perTorrent int64) (budget int64, hardCap int64, maxTorrents int) {
	// No measurable RAM: don't cap by RAM at all. A hard cap of 0 would
	// otherwise make maxTorrents 0 and stop keep-at from holding anything,
	// which is far worse than an uncapped (but still connection-bounded)
	// hold. Callers should log that the RAM budget is disabled.
	if systemTotal <= 0 {
		if perTorrent <= 0 {
			maxTorrents = 0
		} else if userMaxRAM > 0 {
			maxTorrents = int(userMaxRAM / perTorrent)
		} else if configMax > 0 {
			maxTorrents = int(configMax / perTorrent)
		} else {
			maxTorrents = math.MaxInt
		}
		return userMaxRAMOrConfig(userMaxRAM, configMax), 0, maxTorrents
	}

	hardCap = int64(float64(systemTotal) * SystemRAMFractionHardCap)
	if hardCap > systemTotal {
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

	if budget > hardCap {
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

func userMaxRAMOrConfig(userMaxRAM, configMax int64) int64 {
	switch {
	case userMaxRAM > 0:
		return userMaxRAM
	case configMax > 0:
		return configMax
	default:
		return 0
	}
}
