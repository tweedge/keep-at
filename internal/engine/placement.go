package engine

import (
	"fmt"
	"sort"

	"github.com/tweedge/keep-at/internal/config"
)

// onDiskBytes reports the actual on-disk (post-compression) bytes stored in
// a location, staging included. This is what keep-at's space accounting
// subtracts from a location's limit: pieces are gzip-compressed, so the real
// footprint is the on-disk bytes, not the nominal torrent sizes.
func (e *Engine) onDiskBytes(path string) int64 {
	if store, ok := e.stores[path]; ok {
		if n, err := store.DiskUsageAll(); err == nil {
			return n
		}
	}
	return 0
}

// freeBytes reports how much room is left in a storage location, given its
// configured limit and the actual on-disk bytes stored there - so any
// compression gains count toward free space, and a 100 GB torrent that
// compresses to 60 GB only consumes 60 GB of the limit. It's additionally
// capped by what the device actually has free, so block slack and
// filesystem metadata never let accounting drift past real capacity.
func (e *Engine) freeBytes(loc config.StorageLocation) int64 {
	onDisk := e.onDiskBytes(loc.Path)
	free := int64(loc.Limit) - onDisk
	if free < 0 {
		free = 0
	}
	if devFree, err := deviceFreeBytes(loc.Path); err == nil && devFree < free {
		free = devFree
	}
	return free
}

// compressionRatio estimates how much of a torrent's nominal size actually
// lands on disk in a location, based on what keep-at already holds there
// (on-disk bytes / nominal bytes). It's 1.0 when the location holds nothing
// or the data is incompressible, so a fresh location assumes no compression
// until it has evidence.
func (e *Engine) compressionRatio(path string) float64 {
	used := e.state.BytesUsed(path)
	if used <= 0 {
		return 1.0
	}
	onDisk := e.onDiskBytes(path)
	if onDisk <= 0 {
		return 1.0
	}
	ratio := float64(onDisk) / float64(used)
	if ratio > 1.0 {
		ratio = 1.0
	}
	if ratio <= 0 {
		ratio = 1.0
	}
	return ratio
}

// estimatedOnDiskBytes estimates the on-disk footprint a torrent of the
// given nominal size will have in a location, applying that location's
// observed compression ratio. Selection and swap math use this so
// post-compression gains are actually used to fit more torrents, rather than
// being left on the table by comparing nominal sizes against on-disk free.
func (e *Engine) estimatedOnDiskBytes(path string, nominal int64) int64 {
	est := float64(nominal) * e.compressionRatio(path)
	if est < 1 {
		est = 1
	}
	return int64(est)
}

// chooseLocation picks a storage location for a torrent of the given size,
// weighted by how much free space each candidate location, so multiple
// locations fill roughly proportionally rather than one location taking
// every new torrent until it's full. roll is a uniform [0, 1) draw, passed
// in for determinism in tests.
func chooseLocation(locations []config.StorageLocation, freeBytesByPath map[string]int64, sizeBytesByPath map[string]int64, roll float64) (string, error) {
	type candidate struct {
		path string
		free int64
	}

	candidates := make([]candidate, 0, len(locations))
	var totalFree int64
	for _, loc := range locations {
		free := freeBytesByPath[loc.Path]
		if free < sizeBytesByPath[loc.Path] {
			continue
		}
		candidates = append(candidates, candidate{path: loc.Path, free: free})
		totalFree += free
	}

	if len(candidates) == 0 {
		return "", fmt.Errorf("engine: no storage location has enough free space")
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
