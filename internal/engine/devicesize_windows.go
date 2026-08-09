//go:build windows

package engine

import (
	"fmt"
	"log/slog"
	"path/filepath"
	"syscall"

	"github.com/tweedge/keep-at/internal/config"
)

// resolveAllLimits is the Windows implementation. GetDiskFreeSpaceEx reports
// the total bytes of the volume containing the directory, which is the
// formatted capacity of the device.
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

func deviceTotalBytes(path string) (int64, error) {
	dir := path
	for {
		var freeAvail, total, freeTotal uint64
		err := syscall.GetDiskFreeSpaceEx(syscall.StringToUTF16Ptr(dir), &freeAvail, &total, &freeTotal)
		if err == nil {
			return int64(total), nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return 0, fmt.Errorf("no existing ancestor of %s to query", path)
		}
		dir = parent
	}
}

func deviceFreeBytes(path string) (int64, error) {
	dir := path
	for {
		var freeAvail, total, freeTotal uint64
		err := syscall.GetDiskFreeSpaceEx(syscall.StringToUTF16Ptr(dir), &freeAvail, &total, &freeTotal)
		if err == nil {
			return int64(freeAvail), nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return 0, fmt.Errorf("no existing ancestor of %s to query", path)
		}
		dir = parent
	}
}
