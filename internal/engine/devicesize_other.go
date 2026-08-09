//go:build !linux && !darwin && !windows

package engine

import (
	"fmt"
	"log/slog"

	"github.com/tweedge/keep-at/internal/config"
)

// resolveAllLimits is the fallback for platforms keep-at doesn't ship
// release binaries for (freebsd, etc.). It rejects `limit: all` loudly
// rather than silently running without a limit.
func resolveAllLimits(cfg config.Config, logger *slog.Logger) (config.Config, error) {
	for _, loc := range cfg.Storage.Locations {
		if loc.LimitAll {
			return config.Config{}, fmt.Errorf("`limit: all` is not supported on this platform")
		}
	}
	return cfg, nil
}

func deviceTotalBytes(path string) (int64, error) {
	return 0, fmt.Errorf("device size not supported on this platform")
}

func deviceFreeBytes(path string) (int64, error) {
	return 0, fmt.Errorf("device size not supported on this platform")
}
