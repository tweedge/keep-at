// Package netstats tracks what keep-at observes about the wider keep-at
// network while scanning: how many other keep-at nodes it sees, and how
// much data those nodes are collectively seeding versus still downloading.
// It's necessarily an estimate - see Tracker's doc comment - not a census.
package netstats

import (
	"encoding/json"
	"fmt"
	"os"
	"sync"
	"time"
)

// Snapshot is a point-in-time view of the network stats, persisted to disk
// so `keep-at network-status` can read it from a separate process without
// needing to talk to the running daemon directly.
type Snapshot struct {
	ScanStartedAt   time.Time `json:"scan_started_at"`
	ScanCompletedAt time.Time `json:"scan_completed_at"` // zero while a scan is in progress

	TotalCandidates     int `json:"total_candidates"`
	ProcessedCandidates int `json:"processed_candidates"`

	NodeCount     int   `json:"node_count"`
	SeedingBytes  int64 `json:"seeding_bytes"`
	LeechingBytes int64 `json:"leeching_bytes"`
}

// InProgress reports whether the scan this snapshot describes was still
// running when it was written.
func (s Snapshot) InProgress() bool {
	return !s.ScanStartedAt.IsZero() && s.ScanCompletedAt.IsZero()
}

// ProgressPercent returns how far through the scan's candidate list
// processing had gotten, 0-100. Meaningless (returns 0) if TotalCandidates
// is 0.
func (s Snapshot) ProgressPercent() float64 {
	if s.TotalCandidates <= 0 {
		return 0
	}
	pct := float64(s.ProcessedCandidates) / float64(s.TotalCandidates) * 100
	if pct > 100 {
		return 100
	}
	return pct
}

// Load reads a persisted snapshot. A missing file returns a zero Snapshot
// and no error - that just means no scan has run yet.
func Load(path string) (Snapshot, error) {
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return Snapshot{}, nil
	}
	if err != nil {
		return Snapshot{}, fmt.Errorf("netstats: reading %s: %w", path, err)
	}
	var s Snapshot
	if err := json.Unmarshal(data, &s); err != nil {
		return Snapshot{}, fmt.Errorf("netstats: parsing %s: %w", path, err)
	}
	return s, nil
}

// Save atomically persists a snapshot.
func Save(path string, s Snapshot) error {
	data, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return fmt.Errorf("netstats: marshalling snapshot: %w", err)
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return fmt.Errorf("netstats: writing %s: %w", tmp, err)
	}
	if err := os.Rename(tmp, path); err != nil {
		return fmt.Errorf("netstats: finalizing %s: %w", path, err)
	}
	return nil
}

// RuntimeStats is a point-in-time summary of keep-at's own operation -
// how many torrents it's holding and seeding, disk utilization against its
// configured limits, and bytes transferred since boot - persisted to disk
// so `keep-at status` can display it from a separate process without
// needing to talk to the running daemon directly. It complements Snapshot,
// which describes the wider keep-at network observed during a scan.
type RuntimeStats struct {
	CollectedAt time.Time `json:"collected_at"`
	// UptimeSeconds is how long keep-at has been running, for the "since
	// boot" framing of the transfer counters.
	UptimeSeconds int64 `json:"uptime_seconds"`

	HeldTorrents      int `json:"held_torrents"`
	SeedingTorrents   int `json:"seeding_torrents"`
	DownloadingTorrents int `json:"downloading_torrents"`

	// DiskUsedBytes and DiskLimitBytes are sums across every configured
	// storage location (used is tracked against each location's limit, not
	// raw filesystem usage). A zero limit means "not applicable".
	DiskUsedBytes  int64 `json:"disk_used_bytes"`
	DiskLimitBytes int64 `json:"disk_limit_bytes"`

	// UsefulBytesUploaded and UsefulBytesDownloaded are the piece data that
	// actually mattered: bytes keep-at sent to peers that requested it, and
	// bytes it received that it needed. This is the "real" transfer figure.
	//
	// TotalBytesUploaded and TotalBytesDownloaded are everything that moved
	// over peer connections since boot - the useful payload plus protocol
	// overhead, handshakes, and duplicate/wasted chunks received from the
	// swarm. The gap between useful and total is the cost of swarming.
	UsefulBytesUploaded   int64 `json:"useful_bytes_uploaded"`
	UsefulBytesDownloaded int64 `json:"useful_bytes_downloaded"`
	TotalBytesUploaded    int64 `json:"total_bytes_uploaded"`
	TotalBytesDownloaded  int64 `json:"total_bytes_downloaded"`

	// ActivePeers is the number of peer connections keep-at has open right
	// now across all held torrents.
	ActivePeers int `json:"active_peers"`
}

// Uptime returns how long keep-at has been running.
func (s RuntimeStats) Uptime() time.Duration {
	return time.Duration(s.UptimeSeconds) * time.Second
}

// UploadBitsPerSec returns the average total-network upload rate since
// boot, in bits per second (bytes * 8 / uptime).
func (s RuntimeStats) UploadBitsPerSec() float64 {
	if s.UptimeSeconds <= 0 {
		return 0
	}
	return float64(s.TotalBytesUploaded) * 8 / float64(s.UptimeSeconds)
}

// DownloadBitsPerSec returns the average total-network download rate since
// boot, in bits per second (bytes * 8 / uptime).
func (s RuntimeStats) DownloadBitsPerSec() float64 {
	if s.UptimeSeconds <= 0 {
		return 0
	}
	return float64(s.TotalBytesDownloaded) * 8 / float64(s.UptimeSeconds)
}

// DiskUsedPct returns the percentage of keep-at's configured storage
// limits currently in use, or 0 when no limit is set.
func (s RuntimeStats) DiskUsedPct() float64 {
	if s.DiskLimitBytes <= 0 {
		return 0
	}
	pct := float64(s.DiskUsedBytes) / float64(s.DiskLimitBytes) * 100
	if pct > 100 {
		return 100
	}
	return pct
}

// LoadRuntime reads a persisted runtime stats snapshot. A missing file
// returns a zero RuntimeStats and no error - that just means keep-at
// hasn't written one yet.
func LoadRuntime(path string) (RuntimeStats, error) {
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return RuntimeStats{}, nil
	}
	if err != nil {
		return RuntimeStats{}, fmt.Errorf("netstats: reading %s: %w", path, err)
	}
	var s RuntimeStats
	if err := json.Unmarshal(data, &s); err != nil {
		return RuntimeStats{}, fmt.Errorf("netstats: parsing %s: %w", path, err)
	}
	return s, nil
}

// SaveRuntime atomically persists a runtime stats snapshot.
func SaveRuntime(path string, s RuntimeStats) error {
	data, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return fmt.Errorf("netstats: marshalling runtime stats: %w", err)
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return fmt.Errorf("netstats: writing %s: %w", tmp, err)
	}
	if err := os.Rename(tmp, path); err != nil {
		return fmt.Errorf("netstats: finalizing %s: %w", path, err)
	}
	return nil
}

// Tracker accumulates network-wide observations during a single scan.
//
// "How many keep-at nodes are there" and "how much data are they seeding
// or leeching" can't be answered precisely from outside the network - the
// only signal available is what keep-at itself observes while briefly
// joining each candidate torrent's swarm to check for other keep-at peers
// (see engine/probe.go). That means:
//
//   - NodeCount is a count of distinct IP addresses seen claiming to be
//     keep-at, only across torrents keep-at actually scanned this run. A
//     node behind a NAT shared with another keep-at instance undercounts;
//     a node whose address changes between scans overcounts across scans
//     (each Tracker only lives for one scan, so this doesn't compound
//     within a single number, but comparing NodeCount across separate
//     scans isn't a reliable trend line).
//   - SeedingBytes/LeechingBytes sum a torrent's full size once per
//     keep-at node observed holding it complete or incomplete,
//     respectively - deliberately not deduplicated across nodes, since
//     the point is total keep-at-attributable capacity in use, not unique
//     data volume.
type Tracker struct {
	mu            sync.Mutex
	nodes         map[string]struct{}
	seedingBytes  int64
	leechingBytes int64
}

// NewTracker starts a fresh, empty tracker for one scan.
func NewTracker() *Tracker {
	return &Tracker{nodes: make(map[string]struct{})}
}

// Observe records one keep-at peer seen in a torrent's swarm: nodeKey
// identifies the peer (typically its IP address), torrentSize is that
// torrent's total size, and complete is whether the peer has the whole
// torrent (seeding) or not (leeching).
func (t *Tracker) Observe(nodeKey string, torrentSize int64, complete bool) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.nodes[nodeKey] = struct{}{}
	if complete {
		t.seedingBytes += torrentSize
	} else {
		t.leechingBytes += torrentSize
	}
}

func (t *Tracker) NodeCount() int {
	t.mu.Lock()
	defer t.mu.Unlock()
	return len(t.nodes)
}

func (t *Tracker) SeedingBytes() int64 {
	t.mu.Lock()
	defer t.mu.Unlock()
	return t.seedingBytes
}

func (t *Tracker) LeechingBytes() int64 {
	t.mu.Lock()
	defer t.mu.Unlock()
	return t.leechingBytes
}
