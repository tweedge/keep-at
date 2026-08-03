// Package state tracks what keep-at currently holds and seeds, persisted as
// plain JSON so an operator can read it directly if something needs
// investigating - keep-at is meant to run hands-off, but "hands-off" doesn't
// mean "opaque."
package state

import (
	"encoding/json"
	"fmt"
	"os"
	"sync"
	"time"

	"github.com/anacrolix/torrent/metainfo"
)

// Torrent is one item keep-at currently stores and seeds.
type Torrent struct {
	InfoHash        metainfo.Hash `json:"info_hash"`
	Title           string        `json:"title"`
	SizeBytes       int64         `json:"size_bytes"`
	StorageLocation string        `json:"storage_location"`
	AddedAt         time.Time     `json:"added_at"`
	// LastKnownSeeders is refreshed on every scan so the selector can make
	// swap decisions without a fresh scrape of every held torrent on every
	// call.
	LastKnownSeeders int `json:"last_known_seeders"`
}

// State is keep-at's full persisted view of what it's holding. It's safe for
// concurrent use.
type State struct {
	mu      sync.Mutex
	path    string
	Torrent map[string]Torrent `json:"torrents"` // keyed by infohash hex
}

// Load reads state from path, returning an empty State if the file doesn't
// exist yet (a brand new install).
func Load(path string) (*State, error) {
	s := &State{path: path, Torrent: make(map[string]Torrent)}

	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return s, nil
	}
	if err != nil {
		return nil, fmt.Errorf("state: reading %s: %w", path, err)
	}

	var onDisk struct {
		Torrent map[string]Torrent `json:"torrents"`
	}
	if err := json.Unmarshal(data, &onDisk); err != nil {
		return nil, fmt.Errorf("state: parsing %s: %w", path, err)
	}
	if onDisk.Torrent != nil {
		s.Torrent = onDisk.Torrent
	}
	return s, nil
}

// Save atomically writes the current state to disk.
func (s *State) Save() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.saveLocked()
}

func (s *State) saveLocked() error {
	data, err := json.MarshalIndent(struct {
		Torrent map[string]Torrent `json:"torrents"`
	}{Torrent: s.Torrent}, "", "  ")
	if err != nil {
		return fmt.Errorf("state: marshalling: %w", err)
	}

	tmp := s.path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return fmt.Errorf("state: writing %s: %w", tmp, err)
	}
	if err := os.Rename(tmp, s.path); err != nil {
		return fmt.Errorf("state: finalizing %s: %w", s.path, err)
	}
	return nil
}

// Put adds or replaces a held torrent's record and persists the change.
func (s *State) Put(t Torrent) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.Torrent[t.InfoHash.HexString()] = t
	return s.saveLocked()
}

// Remove drops a torrent's record and persists the change.
func (s *State) Remove(infoHash metainfo.Hash) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	delete(s.Torrent, infoHash.HexString())
	return s.saveLocked()
}

// All returns a snapshot of every held torrent.
func (s *State) All() []Torrent {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]Torrent, 0, len(s.Torrent))
	for _, t := range s.Torrent {
		out = append(out, t)
	}
	return out
}

// BytesUsed sums SizeBytes across every held torrent in a given storage
// location, for space accounting against that location's configured limit.
func (s *State) BytesUsed(location string) int64 {
	s.mu.Lock()
	defer s.mu.Unlock()
	var total int64
	for _, t := range s.Torrent {
		if t.StorageLocation == location {
			total += t.SizeBytes
		}
	}
	return total
}
