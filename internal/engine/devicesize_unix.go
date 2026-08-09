//go:build linux || darwin

package engine

import (
	"fmt"
	"log/slog"
	"path/filepath"

	"golang.org/x/sys/unix"

	"github.com/tweedge/keep-at/internal/config"
)

// resolveAllLimits replaces every storage location configured with
// `limit: all` (or `--storage-limit all`) by a concrete byte limit equal to
// config.AllLimitFraction of the device's total (formatted) capacity. It
// returns a copy of cfg with those limits filled in, leaving the original
// untouched so the operator's "all" survives in their config file.
func resolveAllLimits(cfg config.Config, logger *slog.Logger) (config.Config, error) {
	if logger == nil {
		logger = slog.Default()
	}
	for i := range cfg.Storage.Locations {
		loc := &cfg.Storage.Locations[i]
		if !loc.LimitAll {
			continue
		}
		total, err := deviceTotalBytes(loc.Path)
		if err != nil {
			return config.Config{}, fmt.Errorf("resolving `all` limit for %s: %w", loc.Path, err)
		}
		loc.Limit = config.ByteSize(float64(total) * config.AllLimitFraction)
		logger.Info("resolved `all` storage limit",
			"path", loc.Path,
			"device_total", humanBytes(total),
			"limit", loc.Limit.String(),
			"fraction", config.AllLimitFraction)
	}
	return cfg, nil
}

// deviceTotalBytes returns the total (formatted) capacity, in bytes, of the
// filesystem containing path. The location directory may not exist yet, so
// the parent chain is walked up to the first existing directory, which is
// guaranteed to be on the same filesystem the location will be created on.
func deviceTotalBytes(path string) (int64, error) {
	dir := path
	for {
		var st unix.Statfs_t
		if err := unix.Statfs(dir, &st); err == nil {
			return int64(st.Blocks) * int64(st.Bsize), nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return 0, fmt.Errorf("no existing ancestor of %s to stat", path)
		}
		dir = parent
	}
}

// deviceFreeBytes returns the currently free (formatted) capacity, in bytes,
// of the filesystem containing path, used to keep post-compression
// accounting honest against the real device.
func deviceFreeBytes(path string) (int64, error) {
	dir := path
	for {
		var st unix.Statfs_t
		if err := unix.Statfs(dir, &st); err == nil {
			return int64(st.Bavail) * int64(st.Bsize), nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return 0, fmt.Errorf("no existing ancestor of %s to stat", path)
		}
		dir = parent
	}
}
