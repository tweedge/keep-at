package engine

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"testing"
	"time"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/config"
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
		w.Header().Set("Content-Type", "text/xml")
		fmt.Fprint(w, `<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel>`)
		for _, item := range smokeTestItems {
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

	e, err := New(cfg, Options{
		CatalogURL:   catalogServer.URL + "/database.xml",
		ProbeTimeout: 5 * time.Second,
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
