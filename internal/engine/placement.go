package engine

import (
	"fmt"
	"sort"

	"github.com/tweedge/keep-at/internal/config"
)

// freeBytes reports how much room is left in a storage location, given its
// configured limit and what keep-at's state already accounts as used there.
func (e *Engine) freeBytes(loc config.StorageLocation) int64 {
	used := e.state.BytesUsed(loc.Path)
	free := int64(loc.Limit) - used
	if free < 0 {
		return 0
	}
	return free
}

// chooseLocation picks a storage location for a torrent of the given size,
// weighted by how much free space each candidate location, so multiple
// locations fill roughly proportionally rather than one location taking
// every new torrent until it's full. roll is a uniform [0, 1) draw, passed
// in for determinism in tests.
func chooseLocation(locations []config.StorageLocation, freeBytesByPath map[string]int64, sizeBytes int64, roll float64) (string, error) {
	type candidate struct {
		path string
		free int64
	}

	candidates := make([]candidate, 0, len(locations))
	var totalFree int64
	for _, loc := range locations {
		free := freeBytesByPath[loc.Path]
		if free < sizeBytes {
			continue
		}
		candidates = append(candidates, candidate{path: loc.Path, free: free})
		totalFree += free
	}

	if len(candidates) == 0 {
		return "", fmt.Errorf("engine: no storage location has %d bytes free", sizeBytes)
	}

	// Sort for deterministic iteration order given equal weights (map
	// iteration order is otherwise random in Go, which would make the same
	// roll pick different locations across runs).
	sort.Slice(candidates, func(i, j int) bool { return candidates[i].path < candidates[j].path })

	target := roll * float64(totalFree)
	var cursor float64
	for _, c := range candidates {
		cursor += float64(c.free)
		if target < cursor {
			return c.path, nil
		}
	}
	// Floating point rounding can leave target just past the last cursor;
	// fall back to the last candidate rather than erroring out.
	return candidates[len(candidates)-1].path, nil
}
