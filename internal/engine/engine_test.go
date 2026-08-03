package engine

import (
	"bytes"
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/mimisbaeti/internal/config"
)

// buildTestTorrent creates a small single-file torrent on disk and returns
// its parsed MetaInfo, with Announce pointing at a tracker the caller will
// serve themselves.
func buildTestTorrent(t *testing.T, announceURL string, content []byte) *metainfo.MetaInfo {
	t.Helper()

	srcDir := t.TempDir()
	if err := os.WriteFile(filepath.Join(srcDir, "data.bin"), content, 0o644); err != nil {
		t.Fatalf("writing source file: %v", err)
	}

	var info metainfo.Info
	info.PieceLength = 16
	if err := info.BuildFromFilePath(srcDir); err != nil {
		t.Fatalf("BuildFromFilePath: %v", err)
	}

	infoBytes, err := bencode.Marshal(info)
	if err != nil {
		t.Fatalf("marshalling info: %v", err)
	}

	mi := &metainfo.MetaInfo{
		Announce:     announceURL,
		AnnounceList: metainfo.AnnounceList{{announceURL}},
		InfoBytes:    infoBytes,
		CreationDate: time.Now().Add(-30 * 24 * time.Hour).Unix(),
	}
	return mi
}

// academicTorrentsStub serves a minimal database.xml, .torrent download,
// scrape, and announce endpoint good enough to exercise Engine.ScanOnce
// end to end without touching the real Academic Torrents infrastructure.
type academicTorrentsStub struct {
	server   *httptest.Server
	metaInfo *metainfo.MetaInfo
	infoHash metainfo.Hash
	seeders  int
}

func newAcademicTorrentsStub(t *testing.T, title string, content []byte, seeders int) *academicTorrentsStub {
	t.Helper()
	stub := &academicTorrentsStub{seeders: seeders}

	mux := http.NewServeMux()
	stub.server = httptest.NewServer(mux)

	stub.metaInfo = buildTestTorrent(t, stub.server.URL+"/announce.php", content)
	stub.infoHash = stub.metaInfo.HashInfoBytes()

	mux.HandleFunc("/database.xml", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintf(w, `<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel>
<item>
<title>%s</title>
<category>Dataset</category>
<infohash>%s</infohash>
<guid>%s/details/%s</guid>
<link>%s/details/%s</link>
<description>a test dataset</description>
<size>%d</size>
</item>
</channel></rss>`, title, stub.infoHash.HexString(), stub.server.URL, stub.infoHash.HexString(), stub.server.URL, stub.infoHash.HexString(), len(content))
	})

	mux.HandleFunc("/download/", func(w http.ResponseWriter, r *http.Request) {
		var buf bytes.Buffer
		if err := stub.metaInfo.Write(&buf); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		w.Write(buf.Bytes())
	})

	mux.HandleFunc("/scrape.php", func(w http.ResponseWriter, r *http.Request) {
		payload := map[string]interface{}{
			"files": map[string]interface{}{
				string(stub.infoHash.Bytes()): map[string]interface{}{
					"complete":   stub.seeders,
					"downloaded": 0,
					"incomplete": 0,
				},
			},
		}
		var buf bytes.Buffer
		_ = bencode.NewEncoder(&buf).Encode(payload)
		w.Write(buf.Bytes())
	})

	mux.HandleFunc("/announce.php", func(w http.ResponseWriter, r *http.Request) {
		var buf bytes.Buffer
		_ = bencode.NewEncoder(&buf).Encode(map[string]interface{}{
			"interval": 1800,
			"peers":    "",
		})
		w.Write(buf.Bytes())
	})

	return stub
}

func TestScanOnceAddsAvailableCandidate(t *testing.T) {
	content := bytes.Repeat([]byte("x"), 64)
	stub := newAcademicTorrentsStub(t, "Test Dataset", content, 1)
	defer stub.server.Close()

	dataDir := t.TempDir()
	storageDir := t.TempDir()

	cfg := config.Default()
	cfg.DataDir = dataDir
	cfg.Port = 47551
	cfg.Storage.Locations = []config.StorageLocation{{Path: storageDir, Limit: config.ByteSize(1 << 20)}}
	if err := cfg.Validate(); err != nil {
		t.Fatalf("test config should validate: %v", err)
	}

	e, err := New(cfg, Options{
		CatalogURL:              stub.server.URL + "/database.xml",
		AcademicTorrentsBaseURL: stub.server.URL,
		ProbeTimeout:            2 * time.Second,
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer e.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	if err := e.ScanOnce(ctx); err != nil {
		t.Fatalf("ScanOnce: %v", err)
	}

	held := e.state.All()
	if len(held) != 1 {
		t.Fatalf("expected exactly one held torrent after scan, got %d: %+v", len(held), held)
	}
	if held[0].InfoHash != stub.infoHash {
		t.Fatalf("held torrent infohash mismatch: got %s want %s", held[0].InfoHash.HexString(), stub.infoHash.HexString())
	}

	if _, ok := e.torrentClient.Torrent(stub.infoHash); !ok {
		t.Fatalf("expected torrent to be added to the BitTorrent client")
	}
}

func TestScanOnceSkipsUnavailableCandidate(t *testing.T) {
	content := bytes.Repeat([]byte("y"), 64)
	stub := newAcademicTorrentsStub(t, "Zero Seed Dataset", content, 0)
	defer stub.server.Close()

	dataDir := t.TempDir()
	storageDir := t.TempDir()

	cfg := config.Default()
	cfg.DataDir = dataDir
	cfg.Port = 47552
	cfg.Storage.Locations = []config.StorageLocation{{Path: storageDir, Limit: config.ByteSize(1 << 20)}}

	e, err := New(cfg, Options{
		CatalogURL:              stub.server.URL + "/database.xml",
		AcademicTorrentsBaseURL: stub.server.URL,
		ProbeTimeout:            time.Second,
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer e.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := e.ScanOnce(ctx); err != nil {
		t.Fatalf("ScanOnce: %v", err)
	}

	if held := e.state.All(); len(held) != 0 {
		t.Fatalf("expected no torrents held (0 seeders means unavailable), got %+v", held)
	}
}
