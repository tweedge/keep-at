package engine

import (
	"bytes"
	"context"
	"log/slog"
	"testing"
	"time"

	"github.com/tweedge/keep-at/internal/config"
)

// TestCensusRunsPerTorrentProbeLifecycle runs the full census against the
// academicTorrentsStub and verifies two things: the per-torrent probe-client
// lifecycle (create a fresh client per torrent, probe synchronously, close
// it) completes without panics or hangs - the crash mode the old
// concurrent-probe design had - and the resulting census numbers are
// consistent (every torrent in the stub catalog is counted).
func TestCensusRunsPerTorrentProbeLifecycle(t *testing.T) {
	content := bytes.Repeat([]byte("z"), 64)
	stub := newAcademicTorrentsStub(t, "Census Test Dataset", content, 2)
	defer stub.server.Close()

	dataDir := t.TempDir()

	cfg := config.Default()
	cfg.DataDir = dataDir
	cfg.Storage.Locations = []config.StorageLocation{{Path: t.TempDir(), Limit: config.ByteSize(1 << 20)}}
	if err := cfg.Validate(); err != nil {
		t.Fatalf("test config should validate: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	progressCalls := 0
	result, err := RunCensus(ctx, cfg, CensusOptions{
		CatalogURL:              stub.server.URL + "/database.xml",
		AcademicTorrentsBaseURL: stub.server.URL,
		ProbeTimeout:            2 * time.Second,
		Logger:                  slog.New(slog.NewTextHandler(testWriter{t}, nil)),
	}, func(p CensusProgress) { progressCalls++ })
	if err != nil {
		t.Fatalf("RunCensus: %v", err)
	}

	if result.CatalogSize != 1 {
		t.Fatalf("expected 1 catalog torrent, got %d", result.CatalogSize)
	}
	if result.Probed != 1 {
		t.Fatalf("expected 1 torrent probed, got %d (scraped=%d failed=%d)", result.Probed, result.Scraped, result.Failed)
	}
	if result.Scraped != 1 {
		t.Fatalf("expected 1 torrent scraped, got %d", result.Scraped)
	}
	// Seeder floor over the single 2-seeder torrent is 2.
	if result.SeederFloor != 2 {
		t.Fatalf("expected seeder floor 2, got %d", result.SeederFloor)
	}
	// Progress only fires on the every-25 cadence; a 1-torrent census
	// legitimately never reaches it.
	if progressCalls != 0 {
		t.Fatalf("expected no progress callbacks for a 1-torrent census, got %d", progressCalls)
	}
}

type testWriter struct{ t *testing.T }

func (w testWriter) Write(p []byte) (int, error) {
	w.t.Log(string(p))
	return len(p), nil
}
