package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"sort"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/netstats"
	"github.com/tweedge/keep-at/internal/state"
)

// cmdHostedTorrents lists every torrent keep-at currently holds on this host:
// its name, actual on-disk space taken, seeding/downloading status, seeder
// and leecher counts from the last scrape, and a link to its Academic
// Torrents details page. It reads the same persisted files `status` reads
// (state.json, scrape-cache.json, and the storage directories), so it works
// whether or not keep-at is currently running.
func cmdHostedTorrents(args []string) error {
	fs := flag.NewFlagSet("hosted-torrents", flag.ContinueOnError)
	configPath := fs.String("config", "", "path to a config file (optional)")
	if err := fs.Parse(args); err != nil {
		return err
	}

	dir, err := resolveDataDir(*configPath, "")
	if err != nil {
		return err
	}

	st, err := state.Load(dir + "/state.json")
	if err != nil {
		return err
	}
	held := st.All()
	if len(held) == 0 {
		fmt.Println("keep-at is not holding any torrents")
		return nil
	}

	// Last-scrape counts: seeder counts are persisted in state.json and
	// refreshed on every scan; leecher counts live in scrape-cache.json,
	// keyed by infohash. A torrent that isn't in the scrape cache just means
	// it was added since the last scan, so it's reported with zero leechers.
	scrapeCache, err := loadScrapeCache(dir + "/scrape-cache.json")
	if err != nil {
		return err
	}

	rows := make([]hostedRow, 0, len(held))
	for _, t := range held {
		row := hostedRow{
			title:       t.Title,
			infoHash:    t.InfoHash,
			sizeBytes:   t.SizeBytes,
			onDiskBytes: onDiskBytesForTorrent(t),
			seeding:     isSeeding(t),
			seeders:     t.LastKnownSeeders,
			leechers:    scrapeCache[t.InfoHash].leechers,
		}
		rows = append(rows, row)
	}
	sort.Slice(rows, func(i, j int) bool { return rows[i].title < rows[j].title })

	for _, r := range rows {
		status := "downloading"
		if r.seeding {
			status = "seeding"
		}
		fmt.Printf("%s\n", r.title)
		fmt.Printf("  link:        https://academictorrents.com/details/%s\n", r.infoHash.HexString())
		fmt.Printf("  status:      %s\n", status)
		fmt.Printf("  space:       %s on disk (torrent is %s)\n", netstats.HumanBytes(r.onDiskBytes), netstats.HumanBytes(r.sizeBytes))
		fmt.Printf("  last scrape: %d seeders, %d leechers\n", r.seeders, r.leechers)
	}
	return nil
}

type hostedRow struct {
	title       string
	infoHash    metainfo.Hash
	sizeBytes   int64
	onDiskBytes int64
	seeding     bool
	seeders     int
	leechers    int
}

type scrapeCounts struct {
	leechers int
}

// loadScrapeCache reads the persisted per-torrent leecher counts. A missing
// file is fine (no scan has flushed one yet) and returns an empty map.
func loadScrapeCache(path string) (map[metainfo.Hash]scrapeCounts, error) {
	out := make(map[metainfo.Hash]scrapeCounts)
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return out, nil
	}
	if err != nil {
		return nil, fmt.Errorf("reading scrape cache %s: %w", path, err)
	}
		var onDisk struct {
			Entries map[string]struct {
				Seeders  int `json:"seeders"`
				Leechers int `json:"leechers"`
			} `json:"entries"`
		}
		if err := json.Unmarshal(data, &onDisk); err != nil {
			return nil, fmt.Errorf("parsing scrape cache %s: %w", path, err)
		}
		for hex, e := range onDisk.Entries {
			var h metainfo.Hash
			if err := h.FromHexString(hex); err != nil {
				continue
			}
			out[h] = scrapeCounts{leechers: e.Leechers}
		}
		return out, nil
}

// isSeeding reports whether a held torrent has every one of its pieces
// stored on disk. keep-at writes in-progress pieces to a staging/
// subdirectory and moves each verified piece to <index>.piece.gz once it
// completes, so a torrent is seeding once it has at least one final piece
// and staging contains no leftover in-progress pieces. A missing directory
// means it's still downloading its first piece.
func isSeeding(t state.Torrent) bool {
	dir := filepath.Join(t.StorageLocation, t.InfoHash.HexString())
	entries, err := os.ReadDir(dir)
	if err != nil {
		return false
	}
	pieces := 0
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		if len(e.Name()) > len(".piece.gz") && e.Name()[len(e.Name())-len(".piece.gz"):] == ".piece.gz" {
			pieces++
		}
	}
	stagingEntries, err := os.ReadDir(filepath.Join(dir, "staging"))
	if err != nil {
		// No staging dir at all: everything landed in final pieces.
		return pieces > 0
	}
	// Staging exists but must be empty - any file there is an incomplete piece.
	return pieces > 0 && len(stagingEntries) == 0
}

// onDiskBytesForTorrent sums the actual (compressed) bytes a torrent's
// pieces occupy, staging included.
func onDiskBytesForTorrent(t state.Torrent) int64 {
	dir := filepath.Join(t.StorageLocation, t.InfoHash.HexString())
	var total int64
	_ = filepath.Walk(dir, func(_ string, info os.FileInfo, err error) error {
		if err == nil && !info.IsDir() {
			total += info.Size()
		}
		return nil
	})
	return total
}
