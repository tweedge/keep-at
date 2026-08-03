package engine

import (
	"bytes"
	"log/slog"
	"strings"
	"testing"
	"time"
)

func TestLogScrapeProgressIncludesETA(t *testing.T) {
	var buf bytes.Buffer
	logger := slog.New(slog.NewTextHandler(&buf, nil))
	e := &Engine{logger: logger}

	// 10 candidates processed in 10s of "elapsed" time (simulated via a
	// start time in the past) out of 40 total: 1s/candidate so far, 30
	// remaining -> a 30s ETA.
	evalStartedAt := time.Now().Add(-10 * time.Second)
	e.logScrapeProgress(evalStartedAt, 40, 10)

	out := buf.String()
	if !strings.Contains(out, "scrape in progress") {
		t.Fatalf("expected a 'scrape in progress' log line, got: %s", out)
	}
	if !strings.Contains(out, "processed=10") {
		t.Errorf("expected processed=10, got: %s", out)
	}
	if !strings.Contains(out, "total=40") {
		t.Errorf("expected total=40, got: %s", out)
	}
	if !strings.Contains(out, "percent=25%") {
		t.Errorf("expected percent=25%%, got: %s", out)
	}
	if !strings.Contains(out, "eta=") {
		t.Errorf("expected an eta field, got: %s", out)
	}
	// Allow some slack: the test itself takes nonzero wall-clock time, so
	// elapsed might be 10s or a touch more, and ETA scales accordingly.
	if !strings.Contains(out, "eta=3") && !strings.Contains(out, "eta=2") {
		t.Errorf("expected an eta around 30s, got: %s", out)
	}
}

func TestLogScrapeProgressHandlesZeroProcessed(t *testing.T) {
	var buf bytes.Buffer
	logger := slog.New(slog.NewTextHandler(&buf, nil))
	e := &Engine{logger: logger}

	e.logScrapeProgress(time.Now(), 40, 0)

	out := buf.String()
	if !strings.Contains(out, "scrape in progress") {
		t.Fatalf("expected a 'scrape in progress' log line, got: %s", out)
	}
	if strings.Contains(out, "eta=") {
		t.Errorf("did not expect an eta with zero processed candidates, got: %s", out)
	}
}

func TestLogScrapeProgressHandlesUnknownTotal(t *testing.T) {
	var buf bytes.Buffer
	logger := slog.New(slog.NewTextHandler(&buf, nil))
	e := &Engine{logger: logger}

	e.logScrapeProgress(time.Now().Add(-5*time.Second), 0, 3)

	out := buf.String()
	if !strings.Contains(out, "scrape in progress") {
		t.Fatalf("expected a 'scrape in progress' log line, got: %s", out)
	}
	if strings.Contains(out, "eta=") {
		t.Errorf("did not expect an eta with an unknown total, got: %s", out)
	}
}
