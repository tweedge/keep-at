package engine

import (
	"testing"
	"time"

	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/netstats"
)

// TestDelayUntilNextScan verifies that Run won't scan immediately when a
// scan completed recently - the whole point of deferring to the scan
// interval - and that it returns 0 (scan immediately) when the last scan is
// stale or there's no record of one at all.
func TestDelayUntilNextScan(t *testing.T) {
	newEngine := func() *Engine {
		cfg := config.Default()
		cfg.DataDir = t.TempDir()
		return &Engine{cfg: cfg}
	}

	t.Run("no record of a completed scan scans immediately", func(t *testing.T) {
		e := newEngine()
		if got := e.delayUntilNextScan(); got != 0 {
			t.Errorf("delayUntilNextScan with no network-stats = %v, want 0", got)
		}
	})

	t.Run("recent completion waits out the interval", func(t *testing.T) {
		e := newEngine()
		interval := 7 * 24 * time.Hour
		e.cfg.Scan.Interval = config.Duration(interval)
		completedAgo := 6 * time.Hour
		snap := netstats.Snapshot{ScanCompletedAt: time.Now().Add(-completedAgo)}
		if err := netstats.Save(e.networkStatsPath(), snap); err != nil {
			t.Fatalf("Save: %v", err)
		}

		got := e.delayUntilNextScan()
		want := interval - completedAgo
		// Allow a couple seconds of skew from the time between the snapshot
		// write and the call.
		if got < want-2*time.Second || got > want+2*time.Second {
			t.Errorf("delayUntilNextScan = %v, want ~%v", got, want)
		}
	})

	t.Run("stale completion scans immediately", func(t *testing.T) {
		e := newEngine()
		e.cfg.Scan.Interval = config.Duration(7 * 24 * time.Hour)
		snap := netstats.Snapshot{ScanCompletedAt: time.Now().Add(-8 * 24 * time.Hour)}
		if err := netstats.Save(e.networkStatsPath(), snap); err != nil {
			t.Fatalf("Save: %v", err)
		}
		if got := e.delayUntilNextScan(); got != 0 {
			t.Errorf("delayUntilNextScan with stale completion = %v, want 0", got)
		}
	})

	t.Run("zero interval scans immediately", func(t *testing.T) {
		e := newEngine()
		e.cfg.Scan.Interval = 0
		snap := netstats.Snapshot{ScanCompletedAt: time.Now()}
		if err := netstats.Save(e.networkStatsPath(), snap); err != nil {
			t.Fatalf("Save: %v", err)
		}
		if got := e.delayUntilNextScan(); got != 0 {
			t.Errorf("delayUntilNextScan with zero interval = %v, want 0", got)
		}
	})
}
