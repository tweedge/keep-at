package engine

import (
	"runtime"
	"time"

	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/netstats"
)

// runtimeStatsPath is where the current runtime summary is persisted, so a
// separate `keep-at status` invocation can read it without talking to this
// process directly.
func (e *Engine) runtimeStatsPath() string {
	return e.cfg.DataDir + "/runtime-stats.json"
}

// RuntimeStatsPath returns the same path DataDir(cfg) would use, for
// callers (the CLI) that only have a Config, not a running Engine.
func RuntimeStatsPath(cfg config.Config) string {
	return cfg.DataDir + "/runtime-stats.json"
}

// collectRuntimeStats gathers a point-in-time summary of keep-at's own
// operation: what it's holding and seeding, how full its configured storage
// is, and how much it's transferred since boot.
func (e *Engine) collectRuntimeStats() netstats.RuntimeStats {
	held := e.state.All()

	var diskUsed, diskLimit int64
	for _, loc := range e.cfg.Storage.Locations {
		// Disk used is actual on-disk (post-compression) bytes, so the
		// status line matches what the disk really holds - compression gains
		// show up as room, not as phantom usage.
		diskUsed += e.onDiskBytes(loc.Path)
		diskLimit += int64(loc.Limit)
	}

	// Seeding vs downloading is per-torrent: a held torrent keep-at has
	// every piece of (and is therefore willing to upload) counts as seeding,
	// anything else counts as downloading.
	seeding := 0
	for _, t := range e.torrentClient.Torrents() {
		if t.Seeding() {
			seeding++
		}
	}

	stats := e.torrentClient.Stats()

	// Useful data is what actually mattered: pieces sent to peers that
	// requested them, and pieces received that keep-at needed. Total network
	// is everything over peer connections - useful payload plus protocol
	// overhead, handshakes, and duplicate/wasted chunks from the swarm. The
	// gap between the two is the cost of swarming, and it's why a naive
	// "downloaded" figure can far exceed what actually ended up on disk.

	var ms runtime.MemStats
	runtime.ReadMemStats(&ms)

	return netstats.RuntimeStats{
		CollectedAt:           time.Now().UTC(),
		UptimeSeconds:         int64(time.Since(e.startedAt).Seconds()),
		HeldTorrents:          len(held),
		SeedingTorrents:       seeding,
		DiskUsedBytes:         diskUsed,
		DiskLimitBytes:        diskLimit,
		UsefulBytesUploaded:   stats.PeerConns.BytesWrittenData.Int64(),
		UsefulBytesDownloaded: stats.PeerConns.BytesReadUsefulData.Int64(),
		TotalBytesUploaded:    stats.PeerConns.BytesWritten.Int64(),
		TotalBytesDownloaded:  stats.PeerConns.BytesRead.Int64(),
		ActivePeers:           stats.TotalPeers,
		ProcessRSSBytes:       readProcessRSS(),
		HeapAllocBytes:        int64(ms.HeapAlloc),
		Goroutines:            runtime.NumGoroutine(),
	}
}

// logAndSaveRuntimeStats logs a brief summary of what keep-at is doing and
// persists it so `keep-at status` can show it. Called once at startup (after
// held torrents have been resumed) and then periodically.
func (e *Engine) logAndSaveRuntimeStats(kind string) {
	s := e.collectRuntimeStats()

	seeding := s.SeedingTorrents
	downloading := s.HeldTorrents - seeding
	if downloading < 0 {
		downloading = 0
	}

	e.logger.Info("runtime stats",
		"kind", kind,
		"held", s.HeldTorrents,
		"seeding", seeding,
		"downloading", downloading,
		"rss", humanBytes(s.ProcessRSSBytes),
		"heap_alloc", humanBytes(s.HeapAllocBytes),
		"goroutines", s.Goroutines,
		"disk_used", humanBytes(s.DiskUsedBytes),
		"disk_limit", humanBytes(s.DiskLimitBytes),
		"disk_used_pct", s.DiskUsedPct(),
		"uploaded_useful", humanBytes(s.UsefulBytesUploaded),
		"downloaded_useful", humanBytes(s.UsefulBytesDownloaded),
		"uploaded_total", humanBytes(s.TotalBytesUploaded),
		"downloaded_total", humanBytes(s.TotalBytesDownloaded),
		"upload_rate_avg", netstats.HumanBitsPerSec(s.UploadBitsPerSec()),
		"download_rate_avg", netstats.HumanBitsPerSec(s.DownloadBitsPerSec()),
		"peers", s.ActivePeers,
		"uptime", s.Uptime().String(),
	)

	if err := netstats.SaveRuntime(e.runtimeStatsPath(), s); err != nil {
		e.logger.Warn("failed to persist runtime stats", "err", err)
	}
}
