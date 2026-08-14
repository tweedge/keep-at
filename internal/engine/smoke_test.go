package engine

import (
	"bytes"
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"sort"
	"strconv"
	"testing"
	"time"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/atcatalog"
	"github.com/tweedge/keep-at/internal/buildinfo"
	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/netstats"
	"github.com/tweedge/keep-at/internal/state"
)

// smokeTestItem is a real, small, already-seeded Academic Torrents entry,
// hand-picked so this test can complete in seconds instead of the
// significant-time full-catalog scan a real run does. Picked by checking
// real scrape.php results for a handful of the smallest catalog entries
// and keeping the ones with live seeders.
type smokeTestItem struct {
	title    string
	infoHash string
	size     int64
}

var smokeTestItems = []smokeTestItem{
	{"The Relativity of Simultaneity is Wrong.txt", "d137ffd5e951cc53cd789aab935bf8e833bf8229", 1873},
	{"Multiple-Instance Learning of Real-Valued Data", "936a92932c01c3f5e9994ae8bd2115f4ccb4adc9", 7100},
}

// writeSmokeCatalog renders a database.xml document for the given items.
func writeSmokeCatalog(w http.ResponseWriter, items []smokeTestItem) {
	w.Header().Set("Content-Type", "text/xml")
	fmt.Fprint(w, `<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel>`)
	for _, item := range items {
		fmt.Fprintf(w, `<item>
<title>%s</title>
<category>Paper</category>
<infohash>%s</infohash>
<guid>https://academictorrents.com/details/%s</guid>
<link>https://academictorrents.com/details/%s</link>
<description>keep-at smoke test fixture</description>
<size>%d</size>
</item>`, item.title, item.infoHash, item.infoHash, item.infoHash, item.size)
	}
	fmt.Fprint(w, `</channel></rss>`)
}

// TestSmokeRealAcademicTorrents is a real run against the live Academic
// Torrents catalog, capped to a handful of small, already-available
// torrents and 1GB of scratch space in /tmp. It's skipped by default since
// it depends on live network access and real third-party infrastructure;
// set KEEPAT_SMOKE_TEST=1 to run it.
func TestSmokeRealAcademicTorrents(t *testing.T) {
	if os.Getenv("KEEPAT_SMOKE_TEST") != "1" {
		t.Skip("set KEEPAT_SMOKE_TEST=1 to run the real Academic Torrents smoke test")
	}

	mux := http.NewServeMux()
	catalogServer := httptest.NewServer(mux)
	defer catalogServer.Close()

	mux.HandleFunc("/database.xml", func(w http.ResponseWriter, r *http.Request) {
		writeSmokeCatalog(w, smokeTestItems)
	})

	dataDir, err := os.MkdirTemp("/tmp", "keep-at-smoke-data-")
	if err != nil {
		t.Fatalf("creating data dir: %v", err)
	}
	defer os.RemoveAll(dataDir)

	storageDir, err := os.MkdirTemp("/tmp", "keep-at-smoke-storage-")
	if err != nil {
		t.Fatalf("creating storage dir: %v", err)
	}
	defer os.RemoveAll(storageDir)

	cfg := config.Default()
	cfg.DataDir = dataDir
	cfg.Port = 47560
	cfg.Storage.Locations = []config.StorageLocation{{Path: storageDir, Limit: config.ByteSize(1 << 30)}} // 1GB cap
	cfg.Scan.ModerationDelay = config.Duration(0)                                                         // fixtures are years old; no need to wait in the test
	// Optional: exercise API-key attribution end to end when the operator
	// passes their key via env, without hardcoding it into the test.
	if k := os.Getenv("KEEPAT_API_KEY"); k != "" {
		cfg.APIKey = k
	}

	e, err := New(cfg, Options{
		CatalogURL:   catalogServer.URL + "/database.xml",
		// AcademicTorrentsBaseURL left at its default: this test fetches
		// real .torrent files and scrapes the real tracker.
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer e.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Minute)
	defer cancel()

	if err := e.ScanOnce(ctx); err != nil {
		t.Fatalf("ScanOnce: %v", err)
	}

	held := e.state.All()
	if len(held) == 0 {
		t.Fatalf("expected at least one smoke-test torrent to be selected and held")
	}
	t.Logf("keep-at selected %d torrent(s) from the smoke-test catalog", len(held))

	for _, h := range held {
		t.Logf("held: %s (%s, %d bytes)", h.Title, h.InfoHash.HexString(), h.SizeBytes)
		waitForCompletion(t, e, h.InfoHash, 90*time.Second)

		usage, err := e.stores[h.StorageLocation].DiskUsage(h.InfoHash)
		if err != nil {
			t.Fatalf("DiskUsage(%s): %v", h.Title, err)
		}
		if usage <= 0 {
			t.Fatalf("expected nonzero on-disk usage for %s, got %d", h.Title, usage)
		}
		t.Logf("%s: %d bytes on disk (compressed) after real download from Academic Torrents", h.Title, usage)
	}
}

func waitForCompletion(t *testing.T, e *Engine, infoHash metainfo.Hash, timeout time.Duration) {
	t.Helper()
	tr, ok := e.torrentClient.Torrent(infoHash)
	if !ok {
		t.Fatalf("torrent %s not found in client", infoHash.HexString())
	}

	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		<-tr.GotInfo()
		if tr.BytesMissing() == 0 {
			return
		}
		time.Sleep(500 * time.Millisecond)
	}
	t.Fatalf("torrent %s did not finish downloading within %s (missing %d bytes)", infoHash.HexString(), timeout, tr.BytesMissing())
}

// atLeastOneTorrentCompleted reports whether any of the held torrents
// finished downloading within budget. All torrents are polled in parallel
// against a single deadline, so a scan that holds dozens of small torrents
// (as the real-catalog subset test does) finishes its download check in one
// bounded window instead of timeout-after-timeout per torrent.
func atLeastOneTorrentCompleted(e *Engine, held []state.Torrent, timeout time.Duration) ([]string, int64) {
	deadline := time.Now().Add(timeout)
	completed := make([]string, 0)
	var bytesOnDisk int64
	for time.Now().Before(deadline) {
		for _, h := range held {
			tr, ok := e.torrentClient.Torrent(h.InfoHash)
			if !ok {
				continue
			}
			<-tr.GotInfo()
			if tr.BytesMissing() != 0 {
				continue
			}
			usage, err := e.stores[h.StorageLocation].DiskUsage(h.InfoHash)
			if err != nil || usage <= 0 {
				continue
			}
			completed = append(completed, h.Title)
			bytesOnDisk += usage
		}
		if len(completed) > 0 {
			return completed, bytesOnDisk
		}
		time.Sleep(500 * time.Millisecond)
	}
	return nil, 0
}

// TestSmokeRealCatalogSubset is the regression test that the 2-item
// TestSmokeRealAcademicTorrents can't provide: it runs a full scan against a
// real slice of the live Academic Torrents catalog (the smallest entries by
// size, which are the ones most likely to still be seeded), out of /tmp.
//
// Its assertions are deliberately structural rather than "didn't crash", and
// specifically exist to catch scale-dependent regressions like the batched
// multi-hash tracker scrape (which AT's tracker does not support - it returns
// data for only one hash per request, silently dropping the rest):
//
//   - scrape_requests >= eligible: every candidate that made it through
//     evaluation must have actually issued a tracker scrape. Batching scrapes
//     collapses this to ~1 request per N candidates, failing hard.
//   - skipped_scrape_err stays under half of processed: a healthy run sees
//     scrape errors only for genuinely dead torrents, not a majority.
//   - the scan actually finishes (processed == catalog size) within the time
//     budget, so "the scrape completed" is asserted, not assumed.
//   - at least one held torrent completes a real download with data on disk.
//
// Run with:
//
//	KEEPAT_SMOKE_SUBSET=1 go test ./internal/engine/ -run TestSmokeRealCatalogSubset -timeout 15m -v
//
// KEEPAT_SMOKE_SIZE overrides the catalog count (default 100) and
// KEEPAT_SMOKE_RATE overrides the requests/second sent to AT (default 1.0,
// four times keep-at's polite production default - this is an opt-in,
// on-demand smoke test, not a running daemon).
func TestSmokeRealCatalogSubset(t *testing.T) {
	if os.Getenv("KEEPAT_SMOKE_SUBSET") != "1" {
		t.Skip("set KEEPAT_SMOKE_SUBSET=1 to run the real-catalog-subset smoke test")
	}

	size := 100
	if v := os.Getenv("KEEPAT_SMOKE_SIZE"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil || n <= 0 {
			t.Fatalf("KEEPAT_SMOKE_SIZE must be a positive integer, got %q", v)
		}
		size = n
	}
	rate := 1.0
	if v := os.Getenv("KEEPAT_SMOKE_RATE"); v != "" {
		r, err := strconv.ParseFloat(v, 64)
		if err != nil || r <= 0 {
			t.Fatalf("KEEPAT_SMOKE_RATE must be a positive number, got %q", v)
		}
		rate = r
	}

	// Fetch the live catalog and pick the smallest entries: small files are
	// the ones most likely to be genuinely downloadable, which is what the
	// download-completion assertion needs.
	fetchCtx, cancelFetch := context.WithTimeout(context.Background(), time.Minute)
	raw, err := atcatalog.FetchRaw(fetchCtx, &http.Client{Timeout: 30 * time.Second}, atcatalog.DefaultURL, buildinfo.UserAgent())
	cancelFetch()
	if err != nil {
		t.Fatalf("fetching live catalog: %v", err)
	}
	cat, err := atcatalog.Parse(bytes.NewReader(raw))
	if err != nil {
		t.Fatalf("parsing live catalog: %v", err)
	}
	sort.Slice(cat.Items, func(i, j int) bool { return cat.Items[i].SizeBytes < cat.Items[j].SizeBytes })

	seen := map[string]bool{}
	var subset []smokeTestItem
	addItem := func(title, infoHash string, sizeBytes int64) {
		if seen[infoHash] {
			return
		}
		seen[infoHash] = true
		subset = append(subset, smokeTestItem{title: title, infoHash: infoHash, size: sizeBytes})
	}
	for _, it := range cat.Items {
		if it.SizeBytes <= 0 {
			continue
		}
		addItem(it.Title, it.InfoHash.HexString(), it.SizeBytes)
		if len(subset) >= size {
			break
		}
	}
	// Guarantee the verified-downloadable hand-picked items are included, so
	// the "at least one download completes" assertion isn't flaky.
	for _, it := range smokeTestItems {
		addItem(it.title, it.infoHash, it.size)
	}
	if len(subset) == 0 {
		t.Fatal("catalog subset is empty")
	}
	t.Logf("catalog subset: %d entries (smallest by size)", len(subset))

	mux := http.NewServeMux()
	catalogServer := httptest.NewServer(mux)
	defer catalogServer.Close()
	mux.HandleFunc("/database.xml", func(w http.ResponseWriter, r *http.Request) {
		writeSmokeCatalog(w, subset)
	})

	dataDir, err := os.MkdirTemp("/tmp", "keep-at-smoke-subset-data-")
	if err != nil {
		t.Fatalf("creating data dir: %v", err)
	}
	defer os.RemoveAll(dataDir)

	storageDir, err := os.MkdirTemp("/tmp", "keep-at-smoke-subset-storage-")
	if err != nil {
		t.Fatalf("creating storage dir: %v", err)
	}
	defer os.RemoveAll(storageDir)

	cfg := config.Default()
	cfg.DataDir = dataDir
	cfg.Port = 47561
	cfg.Storage.Locations = []config.StorageLocation{{Path: storageDir, Limit: config.ByteSize(1 << 30)}} // 1GB cap
	cfg.Scan.ModerationDelay = config.Duration(0)                                                         // subset torrents are old; no moderation wait in the test
	cfg.Scan.RateLimitPerSecond = rate

	e, err := New(cfg, Options{
		CatalogURL:   catalogServer.URL + "/database.xml",
		// AcademicTorrentsBaseURL left at its default: real .torrent files and
		// real tracker scrapes against live Academic Torrents.
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer e.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 9*time.Minute)
	defer cancel()

	if err := e.ScanOnce(ctx); err != nil {
		t.Fatalf("ScanOnce: %v", err)
	}

	stats := e.lastScanStats
	if stats == nil {
		t.Fatal("expected scan stats to be recorded after ScanOnce")
	}

	eligible := stats.eligible.Load()
	scrapeReqs := stats.scrapeRequests.Load()
	scrapeCached := stats.scrapeCached.Load()
	skippedScrape := stats.skippedScrapeErr.Load()
	processed := eligible + stats.skippedAge.Load() + stats.skippedFetchErr.Load() + skippedScrape

	t.Logf("scan stats: catalog=%d processed=%d eligible=%d scrape_requests=%d scrape_cached=%d skipped_scrape_err=%d",
		len(subset), processed, eligible, scrapeReqs, scrapeCached, skippedScrape)

	// The scan must actually finish within the budget - "the scrape
	// completed" is asserted, not assumed.
	if processed != int64(len(subset)) {
		t.Fatalf("scan did not finish within the time budget: processed %d of %d candidates", processed, len(subset))
	}
	// Every candidate that made it through evaluation must have had a real
	// tracker scrape issued. Batching (which AT's tracker doesn't support)
	// collapses this to ~1 request per N candidates and fails hard.
	if scrapeReqs < eligible {
		t.Fatalf("per-candidate scrape invariant violated: scrape_requests=%d < eligible=%d - tracker scrapes are being dropped or batched in a way AT's tracker doesn't support", scrapeReqs, eligible)
	}
	// A healthy run only sees scrape failures for genuinely dead torrents.
	if skippedScrape > processed/2 {
		t.Fatalf("too many candidates failed to scrape (%d of %d processed) - tracker scraping is broken", skippedScrape, processed)
	}
	// Require a meaningful number of candidates to have survived evaluation,
	// not every last one: the smallest catalog entries include genuinely dead
	// torrents, so a couple of scrape failures are expected against live AT.
	if eligible < 2 {
		t.Fatalf("expected at least 2 eligible candidates, got %d", eligible)
	}

	// The selection gate (see selector.SelectionChance) is unit-tested
	// separately; the smoke test's job is to prove the pipeline works. That
	// gate rightly rejects well-seeded catalog items (keep-at exists to seed
	// minimally-seeded torrents), so relying on it here would make the
	// download check depend on live seeder counts and a random roll. Instead,
	// directly add a couple of the hand-picked, verified-downloadable
	// candidates - whose metadata ScanOnce already fetched and cached - and
	// require one to finish a real download with data on disk. This proves
	// fetch -> scrape -> download -> store works end to end against
	// live infrastructure.
	added := 0
	for _, it := range smokeTestItems {
		md, err := e.fetchMetadata(ctx, mustHash(t, it.infoHash), nil)
		if err != nil {
			t.Logf("could not reload cached metadata for %s: %v", it.title, err)
			continue
		}
		if err := e.AddCandidate(md, storageDir, it.size, it.title); err != nil {
			t.Logf("could not add %s: %v", it.title, err)
			continue
		}
		added++
	}
	if added == 0 {
		t.Fatal("could not add any hand-picked torrent for the download check")
	}

	held := e.state.All()
	completed, bytesOnDisk := atLeastOneTorrentCompleted(e, held, 3*time.Minute)
	if len(completed) == 0 {
		t.Fatal("no added torrent completed a real download from Academic Torrents")
	}
	t.Logf("completed real downloads: %d torrent(s), %s on disk total:", len(completed), netstats.HumanBytes(bytesOnDisk))
	for _, title := range completed {
		t.Logf("  downloaded: %s", title)
	}
}

// mustHash parses a hex infohash, failing the test if it's malformed. It
// exists so smoke-test code can build a metainfo.Hash inline without
// sprinkling error handling through assertions.
func mustHash(t *testing.T, hex string) metainfo.Hash {
	t.Helper()
	var h metainfo.Hash
	if err := h.FromHexString(hex); err != nil {
		t.Fatalf("bad infohash %q: %v", hex, err)
	}
	return h
}
