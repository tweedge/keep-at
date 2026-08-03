package engine

import (
	"testing"

	"github.com/tweedge/keep-at/internal/config"
)

func TestChooseLocationExcludesTooSmall(t *testing.T) {
	locations := []config.StorageLocation{{Path: "/a"}, {Path: "/b"}}
	free := map[string]int64{"/a": 10, "/b": 1000}

	path, err := chooseLocation(locations, free, 500, 0.99)
	if err != nil {
		t.Fatalf("chooseLocation: %v", err)
	}
	if path != "/b" {
		t.Fatalf("expected /b (only one with enough room), got %s", path)
	}
}

func TestChooseLocationErrorsWhenNothingFits(t *testing.T) {
	locations := []config.StorageLocation{{Path: "/a"}}
	free := map[string]int64{"/a": 10}

	if _, err := chooseLocation(locations, free, 500, 0.5); err == nil {
		t.Fatalf("expected an error when no location has enough free space")
	}
}

func TestChooseLocationWeightsByFreeSpace(t *testing.T) {
	locations := []config.StorageLocation{{Path: "/a"}, {Path: "/b"}}
	// /a has 25% of total free space, /b has 75%.
	free := map[string]int64{"/a": 250, "/b": 750}

	path, err := chooseLocation(locations, free, 1, 0.1) // well within /a's share
	if err != nil {
		t.Fatalf("chooseLocation: %v", err)
	}
	if path != "/a" {
		t.Fatalf("roll 0.1 of totalFree=1000 (target=100) should land in /a's [0,250) bucket, got %s", path)
	}

	path, err = chooseLocation(locations, free, 1, 0.9) // well within /b's share
	if err != nil {
		t.Fatalf("chooseLocation: %v", err)
	}
	if path != "/b" {
		t.Fatalf("roll 0.9 of totalFree=1000 (target=900) should land in /b's [250,1000) bucket, got %s", path)
	}
}
