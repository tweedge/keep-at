package engine

import (
	"context"
	"time"
)

// Run scans immediately, then on every Scan.Interval tick thereafter,
// until ctx is cancelled. It's the main loop for `keep-at run`.
func (e *Engine) Run(ctx context.Context) error {
	if err := e.ScanOnce(ctx); err != nil {
		e.logger.Error("initial scan failed", "err", err)
	}

	ticker := time.NewTicker(e.cfg.Scan.Interval.AsDuration())
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return nil
		case <-ticker.C:
			if err := e.ScanOnce(ctx); err != nil {
				e.logger.Error("scan failed", "err", err)
			}
		}
	}
}
