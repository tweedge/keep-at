package engine

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/attorrent"
)

// swarmCache persists per-torrent tracker scrape results across scans, so a
// repeat scan (keep-at rescans weekly) doesn't have to re-query Academic
// Torrents' tracker for every catalog item all over again. Seeder counts
// change slowly relative to a weekly scan, so a cached count is a good enough
// input for the ranking decision most of the time, and a fresh scrape that
// fails (a dead tracker, a transient 429) can fall back to the last known
// count instead of losing the candidate entirely.
//
// The cache is the thing that makes repeat scans cheap: once every torrent's
// scrape is cached, subsequent scans only hit the network for the candidates
// whose cached count has gone stale - not for re-scraping the whole catalog.
type swarmCache struct {
	mu       sync.Mutex
	path     string
	entries  map[string]swarmEntry
	interval time.Duration // entries older than this are treated as stale
}

type swarmEntry struct {
	Seeders   int       `json:"seeders"`
	Leechers  int       `json:"leechers"`
	FetchedAt time.Time `json:"fetched_at"`
}

func newSwarmCache(path string, interval time.Duration) *swarmCache {
	return &swarmCache{
		path:     path,
		entries:  make(map[string]swarmEntry),
		interval: interval,
	}
}

func (c *swarmCache) load() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	data, err := os.ReadFile(c.path)
	if os.IsNotExist(err) {
		return nil
	}
	if err != nil {
		return err
	}
	var onDisk struct {
		Entries map[string]swarmEntry `json:"entries"`
	}
	if err := json.Unmarshal(data, &onDisk); err != nil {
		return fmt.Errorf("engine: parsing swarm cache %s: %w", c.path, err)
	}
	if onDisk.Entries != nil {
		c.entries = onDisk.Entries
	}
	return nil
}

func (c *swarmCache) save() {
	c.mu.Lock()
	defer c.mu.Unlock()
	data, err := json.MarshalIndent(struct {
		Entries map[string]swarmEntry `json:"entries"`
	}{Entries: c.entries}, "", "  ")
	if err != nil {
		return
	}
	if err := os.MkdirAll(filepath.Dir(c.path), 0o755); err != nil {
		return
	}
	tmp := c.path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return
	}
	_ = os.Rename(tmp, c.path)
}

// get returns the cached counts for an infohash if a fresh-enough entry
// exists. "Fresh enough" means fetched within the scan interval, on the
// assumption that a weekly scan doesn't need last week's seeder counts
// refreshed to the second.
func (c *swarmCache) get(infoHash metainfo.Hash) (attorrent.SwarmCounts, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	e, ok := c.entries[infoHash.HexString()]
	if !ok {
		return attorrent.SwarmCounts{}, false
	}
	if c.interval > 0 && time.Since(e.FetchedAt) > c.interval {
		return attorrent.SwarmCounts{}, false
	}
	return attorrent.SwarmCounts{Seeders: e.Seeders, Leechers: e.Leechers}, true
}

// put records a freshly-scraped count.
func (c *swarmCache) put(infoHash metainfo.Hash, counts attorrent.SwarmCounts) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.entries[infoHash.HexString()] = swarmEntry{
		Seeders:   counts.Seeders,
		Leechers:  counts.Leechers,
		FetchedAt: time.Now(),
	}
}
