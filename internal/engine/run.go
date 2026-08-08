package engine

import (
	"context"
	"time"
)

// Run scans immediately, then on every Scan.Interval tick thereafter,
// until ctx is cancelled. It's the main loop for `keep-at run`.
//
// It also emits a runtime summary (see logAndSaveRuntimeStats) once at
// startup - by which point held torrents have already been resumed by New -
// and then every StatsInterval, so operators and `keep-at status` have a
// steady picture of what keep-at is doing even between scans. The periodic
// summary runs on its own goroutine so it keeps ticking even while a
// long-running scan occupies the main loop.
func (e *Engine) Run(ctx context.Context) error {
	e.logAndSaveRuntimeStats("startup")

	stopStats := e.startRuntimeStatsLoop(ctx)
	defer stopStats()

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

// startRuntimeStatsLoop launches the periodic runtime-summary goroutine and
// returns a stop function for it. It does nothing (and returns a no-op) when
// StatsInterval is zero, i.e. periodic summaries are disabled. The goroutine
// also exits on its own when ctx is done.
func (e *Engine) startRuntimeStatsLoop(ctx context.Context) func() {
	interval := e.cfg.StatsInterval.AsDuration()
	if interval <= 0 {
		return func() {}
	}

	stop := make(chan struct{})
	go func() {
		ticker := time.NewTicker(interval)
		defer ticker.Stop()
		for {
			select {
			case <-stop:
				return
			case <-ctx.Done():
				return
			case <-ticker.C:
				e.logAndSaveRuntimeStats("periodic")
			}
		}
	}()

	return func() { close(stop) }
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
