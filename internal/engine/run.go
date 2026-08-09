package engine

import (
	"context"
	"time"

	"github.com/tweedge/keep-at/internal/netstats"
)

// Run scans on the Scan.Interval cadence until ctx is cancelled. It's the
// main loop for `keep-at run`. The first scan runs right away only if the
// last completed scan was more than an interval ago (or there's no record
// of one); if a scan completed recently - e.g. keep-at restarted shortly
// after one finished - it waits out the remainder of the interval instead
// of immediately scraping again.
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

	if delay := e.delayUntilNextScan(); delay > 0 {
		e.logger.Info("next scan is not due yet; waiting instead of scanning immediately",
			"delay", humanDuration(delay))
		select {
		case <-ctx.Done():
			return nil
		case <-time.After(delay):
		}
	}

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

// delayUntilNextScan returns how long to wait before the next scan should
// run, based on when the last one completed and the configured scan
// interval. It returns 0 when there's no record of a completed scan (a
// first run, or a previous process that died mid-scan), or when the next
// scan is already due.
func (e *Engine) delayUntilNextScan() time.Duration {
	interval := e.cfg.Scan.Interval.AsDuration()
	if interval <= 0 {
		return 0
	}
	snap, err := netstats.Load(e.networkStatsPath())
	if err != nil || snap.ScanCompletedAt.IsZero() {
		return 0
	}
	remaining := time.Until(snap.ScanCompletedAt.Add(interval))
	if remaining < 0 {
		return 0
	}
	return remaining
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
