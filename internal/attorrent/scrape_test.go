package attorrent

import (
	"bytes"
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"testing"

	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
)

func TestDeriveHTTPScrapeURL(t *testing.T) {
	cases := []struct {
		announce string
		want     string
		ok       bool
	}{
		{"https://academictorrents.com/announce.php", "https://academictorrents.com/scrape.php", true},
		{"http://example.com/announce", "http://example.com/scrape", true},
		{"http://example.com/tracker/announce", "http://example.com/tracker/scrape", true},
		{"http://example.com/", "", false},
		{"http://example.com/tracker.php", "", false},
	}
	for _, c := range cases {
		got, ok := deriveHTTPScrapeURL(c.announce)
		if ok != c.ok || got != c.want {
			t.Errorf("deriveHTTPScrapeURL(%q) = (%q, %v), want (%q, %v)", c.announce, got, ok, c.want, c.ok)
		}
	}
}

func TestScrapeHTTPParsesBencodeResponse(t *testing.T) {
	var hash metainfo.Hash
	if err := hash.FromHexString("30ac2ef27829b1b5a7d0644097f55f335ca5241b"); err != nil {
		t.Fatalf("FromHexString: %v", err)
	}

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/scrape.php" {
			t.Errorf("expected /scrape.php, got %s", r.URL.Path)
		}
		payload := map[string]interface{}{
			"files": map[string]interface{}{
				string(hash.Bytes()): map[string]interface{}{
					"complete":   3,
					"downloaded": 100,
					"incomplete": 2,
				},
			},
		}
		var buf bytes.Buffer
		if err := bencode.NewEncoder(&buf).Encode(payload); err != nil {
			t.Fatalf("encoding fixture response: %v", err)
		}
		_, _ = w.Write(buf.Bytes())
	}))
	defer server.Close()

	counts, err := ScrapeHTTP(context.Background(), server.Client(), "", server.URL+"/announce.php", []metainfo.Hash{hash})
	if err != nil {
		t.Fatalf("ScrapeHTTP: %v", err)
	}
	got, ok := counts[hash]
	if !ok {
		t.Fatalf("expected counts for hash, got none")
	}
	if got.Seeders != 3 || got.Leechers != 2 || got.Completed != 100 {
		t.Fatalf("unexpected counts: %+v", got)
	}
}

// TestScrapeHTTPLiveAgainstAcademicTorrents is a smoke test against the
// real academictorrents.com tracker. It only runs when KEEPAT_LIVE_TEST=1 is
// set, so normal `go test` runs never depend on network access.
func TestScrapeHTTPLiveAgainstAcademicTorrents(t *testing.T) {
	if os.Getenv("KEEPAT_LIVE_TEST") != "1" {
		t.Skip("set KEEPAT_LIVE_TEST=1 to run this against the real academictorrents.com tracker")
	}

	var hash metainfo.Hash
	if err := hash.FromHexString("30ac2ef27829b1b5a7d0644097f55f335ca5241b"); err != nil {
		t.Fatalf("FromHexString: %v", err)
	}

	counts, err := ScrapeHTTP(context.Background(), http.DefaultClient, "", "https://academictorrents.com/announce.php", []metainfo.Hash{hash})
	if err != nil {
		t.Fatalf("ScrapeHTTP: %v", err)
	}
	if _, ok := counts[hash]; !ok {
		t.Fatalf("expected a result for the known infohash")
	}
	t.Logf("live scrape result: %+v", counts[hash])
}
