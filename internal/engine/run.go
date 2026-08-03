package engine

import (
	"context"
	"time"
)

// Run scans immediately, then on every Scan.Interval tick thereafter,
// until ctx is cancelled. It's the main loop for `keep-at run`.
func (e *Engine) Run(ctx context.Context) error {
	e.runScanLogged(ctx, "initial")

	ticker := time.NewTicker(e.cfg.Scan.Interval.AsDuration())
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return nil
		case <-ticker.C:
			e.runScanLogged(ctx, "periodic")
		}
	}
}

// runScanLogged wraps ScanOnce with start/duration logging - full catalog
// scans can run long, and knowing how long one actually took (and whether
// it finished or errored) matters for diagnosing a scan that's stuck or
// one that's crashing partway through.
func (e *Engine) runScanLogged(ctx context.Context, kind string) {
	e.logger.Info("scan starting", "kind", kind)
	startedAt := time.Now()

	err := e.ScanOnce(ctx)

	duration := time.Since(startedAt)
	if err != nil {
		e.logger.Error("scan failed", "kind", kind, "duration", duration.String(), "err", err)
		return
	}
	e.logger.Info("scan completed", "kind", kind, "duration", duration.String())
}
