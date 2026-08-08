package engine

import (
	"testing"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/atcatalog"
	"github.com/tweedge/keep-at/internal/state"
)

func catalogItem(title, hexHash string) atcatalog.Item {
	var hash metainfo.Hash
	_ = hash.FromHexString(hexHash)
	return atcatalog.Item{Title: title, InfoHash: hash}
}

func catalogOf(items ...atcatalog.Item) atcatalog.Catalog {
	return atcatalog.Catalog{Items: items}
}

func heldTorrent(name string, seeders int, size int64) state.Torrent {
	return state.Torrent{
		InfoHash:         metainfo.HashBytes([]byte(name)),
		Title:            name,
		SizeBytes:        size,
		LastKnownSeeders: seeders,
	}
}

func TestSelectDisplaceableSingleTorrentSuffices(t *testing.T) {
	held := []state.Torrent{
		heldTorrent("big-healthy", 100, 1000),
		heldTorrent("small-fragile", 4, 10),
	}

	// candidate has 1 seeder, margin 3: needs held seeders >= 4 to qualify.
	got := selectDisplaceable(held, 1, 500, 3, false)
	if len(got) != 1 || got[0].Title != "big-healthy" {
		t.Fatalf("expected to displace only big-healthy, got %+v", got)
	}
}

func TestSelectDisplaceableCombinesMultipleQualifyingTorrents(t *testing.T) {
	held := []state.Torrent{
		heldTorrent("healthy-a", 50, 300),
		heldTorrent("healthy-b", 40, 300),
		heldTorrent("healthy-c", 30, 300),
		heldTorrent("too-fragile", 2, 1000), // doesn't individually qualify, must be excluded
	}

	// candidate has 1 seeder, margin 3: needs held seeders >= 4 to qualify.
	// None of a/b/c alone (300 bytes) covers a candidate needing 700, but
	// two of them together (600) still don't; all three (900) do.
	got := selectDisplaceable(held, 1, 700, 3, false)
	if len(got) != 3 {
		t.Fatalf("expected all three qualifying torrents combined, got %d: %+v", len(got), got)
	}
	for _, h := range got {
		if h.Title == "too-fragile" {
			t.Fatalf("too-fragile should never be selected: it doesn't individually clear the margin")
		}
	}

	var total int64
	for _, h := range got {
		total += h.SizeBytes
	}
	if total < 700 {
		t.Fatalf("combined size %d does not cover the 700 needed", total)
	}
}

func TestSelectDisplaceablePrefersFewestNeeded(t *testing.T) {
	held := []state.Torrent{
		heldTorrent("plenty-big", 90, 1000),
		heldTorrent("also-qualifies", 80, 1000),
	}

	got := selectDisplaceable(held, 1, 500, 3, false)
	if len(got) != 1 {
		t.Fatalf("expected a single torrent to suffice, got %d: %+v", len(got), got)
	}
	if got[0].Title != "plenty-big" {
		t.Fatalf("expected the most-seeded (least valuable to keep) torrent chosen first, got %s", got[0].Title)
	}
}

func TestSelectDisplaceableReturnsNilWhenNothingQualifies(t *testing.T) {
	held := []state.Torrent{
		heldTorrent("too-good-to-lose", 2, 1000),
	}
	got := selectDisplaceable(held, 1, 500, 3, false)
	if got != nil {
		t.Fatalf("expected nil, got %+v", got)
	}
}

func TestSelectDisplaceableReturnsNilWhenEvenAllQualifiersArentEnough(t *testing.T) {
	held := []state.Torrent{
		heldTorrent("small-a", 50, 10),
		heldTorrent("small-b", 40, 10),
	}
	got := selectDisplaceable(held, 1, 1000, 3, false)
	if got != nil {
		t.Fatalf("expected nil when even every qualifying torrent combined isn't enough room, got %+v", got)
	}
}

func TestCountPendingCandidatesExcludesHeldAndBlocked(t *testing.T) {
	blocked := stubBlocklist{blockedTitles: map[string]bool{"Blocked Title": true}}

	catalog := catalogOf(
		catalogItem("Held Item", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
		catalogItem("Blocked Title", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
		catalogItem("Fresh Candidate", "cccccccccccccccccccccccccccccccccccccccc"),
	)
	heldHashes := map[string]bool{"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": true}

	got := countPendingCandidates(catalog, heldHashes, blocked)
	if got != 1 {
		t.Fatalf("expected exactly 1 pending candidate, got %d", got)
	}
}

type stubBlocklist struct {
	blockedTitles map[string]bool
}

func (b stubBlocklist) Blocks(title, _ string) (bool, string) {
	return b.blockedTitles[title], ""
}
