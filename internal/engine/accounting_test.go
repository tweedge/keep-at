package engine

import (
	"math/rand"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/state"
)

// newTestEngine builds an Engine with real (but empty) storage and a real
// torrent client, for exercising behavior that touches state, stores, and
// RemoveTorrent without needing any network or catalog.
func newTestEngine(t *testing.T, timeout time.Duration) (*Engine, string) {
	t.Helper()
	dataDir := t.TempDir()
	storageDir := t.TempDir()

	cfg := config.Default()
	cfg.DataDir = dataDir
	cfg.Port = 20000 + rand.Intn(15000) // avoid collisions between parallel tests
	cfg.Storage.Locations = []config.StorageLocation{{Path: storageDir, Limit: config.ByteSize(1 << 30)}}
	cfg.Scan.StallEvictionTimeout = config.Duration(timeout)

	e, err := New(cfg, Options{})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	t.Cleanup(func() { _ = e.Close() })
	return e, storageDir
}

func TestEvictStalledTorrentsRemovesOnlyZeroSeederNoProgressTorrents(t *testing.T) {
	e, storageDir := newTestEngine(t, 14*24*time.Hour)

	mustPut := func(name string, seeders int, lastProgress time.Time) metainfo.Hash {
		h := metainfo.HashBytes([]byte(name))
		tr := state.Torrent{
			InfoHash:         h,
			Title:            name,
			SizeBytes:        1000,
			StorageLocation:  storageDir,
			LastKnownSeeders: seeders,
			CompletedPieces:  3,
			LastProgressAt:   lastProgress,
		}
		if err := e.state.Put(tr); err != nil {
			t.Fatalf("Put(%s): %v", name, err)
		}
		return h
	}

	// Stalled: 0 seeders, no progress for well over the timeout.
	stalledHash := mustPut("stalled", 0, time.Now().UTC().Add(-30*24*time.Hour))
	// Healthy despite zero seeders: progress within the timeout.
	recentHash := mustPut("recent-progress", 0, time.Now().UTC().Add(-time.Hour))
	// Has seeders: not stalled no matter how long since progress.
	seededHash := mustPut("seeded", 5, time.Now().UTC().Add(-30*24*time.Hour))
	// No stall clock yet (never observed): must not be touched.
	unobservedHash := mustPut("unobserved", 0, time.Time{})

	catalogHashes := map[string]bool{
		stalledHash.HexString():    true,
		recentHash.HexString():     true,
		seededHash.HexString():     true,
		unobservedHash.HexString(): true,
	}

	e.evictStalledTorrents(catalogHashes)

	held := e.state.All()
	byName := make(map[string]bool, len(held))
	for _, h := range held {
		byName[h.Title] = true
	}
	if byName["stalled"] {
		t.Errorf("expected stalled torrent to be evicted")
	}
	if !byName["recent-progress"] {
		t.Errorf("expected recent-progress torrent to survive")
	}
	if !byName["seeded"] {
		t.Errorf("expected seeded torrent to survive")
	}
	if !byName["unobserved"] {
		t.Errorf("expected unobserved torrent to survive (no stall clock yet)")
	}
}

func TestEvictStalledTorrentsDisabledByZeroTimeout(t *testing.T) {
	e, storageDir := newTestEngine(t, 0)

	h := metainfo.HashBytes([]byte("stalled-disabled"))
	if err := e.state.Put(state.Torrent{
		InfoHash:         h,
		Title:            "stalled-disabled",
		SizeBytes:        1000,
		StorageLocation:  storageDir,
		LastKnownSeeders: 0,
		LastProgressAt:   time.Now().UTC().Add(-30 * 24 * time.Hour),
	}); err != nil {
		t.Fatalf("Put: %v", err)
	}

	e.evictStalledTorrents(map[string]bool{h.HexString(): true})

	if len(e.state.All()) != 1 {
		t.Fatalf("expected stalled torrent to survive when eviction is disabled, got %d held", len(e.state.All()))
	}
}

func TestResolveAllLimitsUsesDeviceFraction(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("MkdirAll: %v", err)
	}

	cfg := config.Default()
	cfg.Storage.Locations = []config.StorageLocation{{Path: filepath.Join(dir, "sub", "storage"), LimitAll: true}}

	resolved, err := resolveAllLimits(cfg, nil)
	if err != nil {
		t.Fatalf("resolveAllLimits: %v", err)
	}
	loc := resolved.Storage.Locations[0]
	if loc.Limit <= 0 {
		t.Fatalf("expected a concrete positive limit, got %d", loc.Limit)
	}

	// The resolved limit must be a safe fraction of the device's total, not
	// the whole device.
	total, err := deviceTotalBytes(filepath.Join(dir, "sub", "storage"))
	if err != nil {
		t.Fatalf("deviceTotalBytes: %v", err)
	}
	if int64(loc.Limit) >= total {
		t.Errorf("resolved limit %d should be strictly less than device total %d", loc.Limit, total)
	}

	// The operator's config must still say "all" - resolution returns a copy.
	if !cfg.Storage.Locations[0].LimitAll {
		t.Errorf("original config should keep LimitAll after resolution")
	}
}

func TestEstimatedOnDiskBytesAppliesCompressionRatio(t *testing.T) {
	e, _ := newTestEngine(t, 14*24*time.Hour)

	// Empty location: no compression observed yet, so the estimate is the
	// nominal size.
	if got := e.estimatedOnDiskBytes(e.cfg.Storage.Locations[0].Path, 1000); got != 1000 {
		t.Errorf("empty-location estimate = %d, want 1000", got)
	}

	// A location where on-disk usage is half of nominal (50% compression)
	// should estimate half the candidate's nominal size. Fake the state's
	// nominal sum and the store's on-disk usage: state.BytesUsed sums
	// SizeBytes, and onDiskBytes reads from the store.
	h := metainfo.HashBytes([]byte("compressed-held"))
	if err := e.state.Put(state.Torrent{
		InfoHash:        h,
		Title:           "compressed-held",
		SizeBytes:       2000,
		StorageLocation: e.cfg.Storage.Locations[0].Path,
	}); err != nil {
		t.Fatalf("Put: %v", err)
	}

	// Simulate the on-disk side: write a small file under the torrent's
	// storage dir so DiskUsageAll sees ~1000 bytes of compressed data. (The
	// store keys by infohash hex.)
	torrentDir := filepath.Join(e.cfg.Storage.Locations[0].Path, h.HexString())
	if err := os.MkdirAll(torrentDir, 0o755); err != nil {
		t.Fatalf("MkdirAll: %v", err)
	}
	if err := os.WriteFile(filepath.Join(torrentDir, "0.piece.gz"), make([]byte, 1000), 0o644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}

	got := e.estimatedOnDiskBytes(e.cfg.Storage.Locations[0].Path, 1000)
	if got != 500 {
		t.Errorf("estimate with 50%% compression = %d, want 500", got)
	}
}
