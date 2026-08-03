package attorrent

import (
	"os"
	"testing"
	"time"
)

func TestParseTorrentBytesExtractsCreationDateAndTrackers(t *testing.T) {
	body, err := os.ReadFile("testdata/wikipedia-2013.torrent")
	if err != nil {
		t.Fatalf("reading fixture: %v", err)
	}

	md, err := ParseTorrentBytes(body)
	if err != nil {
		t.Fatalf("ParseTorrentBytes: %v", err)
	}

	wantInfoHash := "30ac2ef27829b1b5a7d0644097f55f335ca5241b"
	if md.InfoHash.HexString() != wantInfoHash {
		t.Fatalf("infohash mismatch: got %s want %s", md.InfoHash.HexString(), wantInfoHash)
	}

	wantCreated := time.Date(2013, 8, 8, 13, 49, 17, 0, time.UTC)
	if !md.CreatedAt.Equal(wantCreated) {
		t.Fatalf("creation date mismatch: got %v want %v", md.CreatedAt, wantCreated)
	}

	if len(md.Trackers) == 0 {
		t.Fatalf("expected at least one tracker")
	}
	found := false
	for _, tr := range md.Trackers {
		if tr == "https://academictorrents.com/announce.php" {
			found = true
		}
	}
	if !found {
		t.Fatalf("expected academictorrents.com announce URL among trackers, got %v", md.Trackers)
	}
}
