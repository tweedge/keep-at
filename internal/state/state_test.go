package state

import (
	"path/filepath"
	"testing"

	"github.com/anacrolix/torrent/metainfo"
)

func TestPutRemoveAndReload(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "state.json")

	s, err := Load(path)
	if err != nil {
		t.Fatalf("Load (fresh): %v", err)
	}
	if len(s.All()) != 0 {
		t.Fatalf("expected empty state on fresh load")
	}

	hash := metainfo.HashBytes([]byte("torrent-a"))
	if err := s.Put(Torrent{InfoHash: hash, Title: "A", SizeBytes: 1000, StorageLocation: "/data"}); err != nil {
		t.Fatalf("Put: %v", err)
	}

	reloaded, err := Load(path)
	if err != nil {
		t.Fatalf("Load (after put): %v", err)
	}
	all := reloaded.All()
	if len(all) != 1 || all[0].Title != "A" {
		t.Fatalf("unexpected reloaded state: %+v", all)
	}

	if err := reloaded.Remove(hash); err != nil {
		t.Fatalf("Remove: %v", err)
	}
	rereloaded, err := Load(path)
	if err != nil {
		t.Fatalf("Load (after remove): %v", err)
	}
	if len(rereloaded.All()) != 0 {
		t.Fatalf("expected state empty after remove, got %+v", rereloaded.All())
	}
}

func TestBytesUsedSumsPerLocation(t *testing.T) {
	dir := t.TempDir()
	s, err := Load(filepath.Join(dir, "state.json"))
	if err != nil {
		t.Fatalf("Load: %v", err)
	}

	mustPut := func(name string, size int64, loc string) {
		h := metainfo.HashBytes([]byte(name))
		if err := s.Put(Torrent{InfoHash: h, Title: name, SizeBytes: size, StorageLocation: loc}); err != nil {
			t.Fatalf("Put(%s): %v", name, err)
		}
	}
	mustPut("a", 100, "/disk1")
	mustPut("b", 200, "/disk1")
	mustPut("c", 50, "/disk2")

	if got := s.BytesUsed("/disk1"); got != 300 {
		t.Errorf("BytesUsed(/disk1) = %d, want 300", got)
	}
	if got := s.BytesUsed("/disk2"); got != 50 {
		t.Errorf("BytesUsed(/disk2) = %d, want 50", got)
	}
	if got := s.BytesUsed("/nonexistent"); got != 0 {
		t.Errorf("BytesUsed(/nonexistent) = %d, want 0", got)
	}
}
