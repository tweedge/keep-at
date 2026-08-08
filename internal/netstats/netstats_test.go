package netstats

import (
	"path/filepath"
	"testing"
	"time"
)

func TestTrackerAccumulatesDistinctNodesAndBytes(t *testing.T) {
	tr := NewTracker()
	tr.Observe("1.2.3.4", 1000, true) // seeding
	tr.Observe("1.2.3.4", 500, false) // same node, leeching a different torrent
	tr.Observe("5.6.7.8", 2000, true) // a second node, also seeding

	if got := tr.NodeCount(); got != 2 {
		t.Errorf("NodeCount() = %d, want 2", got)
	}
	if got := tr.SeedingBytes(); got != 3000 {
		t.Errorf("SeedingBytes() = %d, want 3000", got)
	}
	if got := tr.LeechingBytes(); got != 500 {
		t.Errorf("LeechingBytes() = %d, want 500", got)
	}
}

func TestSnapshotProgressPercent(t *testing.T) {
	cases := []struct {
		name string
		snap Snapshot
		want float64
	}{
		{"no candidates known yet", Snapshot{}, 0},
		{"halfway", Snapshot{TotalCandidates: 10, ProcessedCandidates: 5}, 50},
		{"complete", Snapshot{TotalCandidates: 10, ProcessedCandidates: 10}, 100},
		{"clamped past 100", Snapshot{TotalCandidates: 10, ProcessedCandidates: 12}, 100},
	}
	for _, c := range cases {
		if got := c.snap.ProgressPercent(); got != c.want {
			t.Errorf("%s: ProgressPercent() = %v, want %v", c.name, got, c.want)
		}
	}
}

func TestSnapshotInProgress(t *testing.T) {
	now := time.Now()
	inProgress := Snapshot{ScanStartedAt: now}
	if !inProgress.InProgress() {
		t.Errorf("expected InProgress() true when ScanCompletedAt is zero")
	}

	done := Snapshot{ScanStartedAt: now, ScanCompletedAt: now.Add(time.Minute)}
	if done.InProgress() {
		t.Errorf("expected InProgress() false once ScanCompletedAt is set")
	}

	if (Snapshot{}).InProgress() {
		t.Errorf("expected a totally empty snapshot to not be 'in progress'")
	}
}

func TestSaveAndLoadRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "network-stats.json")

	loaded, err := Load(path)
	if err != nil {
		t.Fatalf("Load (missing file): %v", err)
	}
	if loaded != (Snapshot{}) {
		t.Fatalf("expected zero snapshot for a missing file, got %+v", loaded)
	}

	want := Snapshot{
		ScanStartedAt:       time.Now().UTC().Truncate(time.Second),
		TotalCandidates:     42,
		ProcessedCandidates: 10,
		NodeCount:           3,
		SeedingBytes:        123456,
		LeechingBytes:       7890,
	}
	if err := Save(path, want); err != nil {
		t.Fatalf("Save: %v", err)
	}

	got, err := Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if !got.ScanStartedAt.Equal(want.ScanStartedAt) || got.TotalCandidates != want.TotalCandidates ||
		got.ProcessedCandidates != want.ProcessedCandidates || got.NodeCount != want.NodeCount ||
		got.SeedingBytes != want.SeedingBytes || got.LeechingBytes != want.LeechingBytes {
		t.Fatalf("round trip mismatch: got %+v, want %+v", got, want)
	}
}

func TestHumanBytes(t *testing.T) {
	cases := []struct {
		bytes int64
		want  string
	}{
		{0, "0 B"},
		{512, "512 B"},
		{1536, "1.5 KB"},
		{1 << 20, "1.0 MB"},
		{int64(1.5 * (1 << 30)), "1.5 GB"},
		{int64(2.25 * (1 << 40)), "2.2 TB"},
	}
	for _, c := range cases {
		if got := HumanBytes(c.bytes); got != c.want {
			t.Errorf("HumanBytes(%d) = %q, want %q", c.bytes, got, c.want)
		}
	}
}

func TestRuntimeStatsRoundTrip(t *testing.T) {
	dir := t.TempDir()
	path := dir + "/runtime-stats.json"

	if _, err := LoadRuntime(path); err != nil {
		t.Fatalf("LoadRuntime on missing file: %v", err)
	}

	want := RuntimeStats{
		CollectedAt:      time.Now().UTC(),
		UptimeSeconds:    3600,
		HeldTorrents:     12,
		SeedingTorrents:  10,
		DownloadingTorrents: 2,
		DiskUsedBytes:    50 << 30,
		DiskLimitBytes:   100 << 30,
		BytesUploaded:    5 << 30,
		BytesDownloaded:  1 << 30,
		ActivePeers:      24,
	}
	if err := SaveRuntime(path, want); err != nil {
		t.Fatalf("SaveRuntime: %v", err)
	}

	got, err := LoadRuntime(path)
	if err != nil {
		t.Fatalf("LoadRuntime: %v", err)
	}
	if got.HeldTorrents != want.HeldTorrents || got.SeedingTorrents != want.SeedingTorrents ||
		got.DiskUsedBytes != want.DiskUsedBytes || got.Uptime() != time.Hour {
		t.Errorf("round trip mismatch: got %+v want %+v", got, want)
	}
}

func TestRuntimeStatsDiskUsedPct(t *testing.T) {
	cases := []struct {
		name     string
		used     int64
		limit    int64
		wantPct  float64
	}{
		{"half full", 50 << 30, 100 << 30, 50},
		{"over limit", 120 << 30, 100 << 30, 100},
		{"no limit", 10 << 30, 0, 0},
	}
	for _, c := range cases {
		s := RuntimeStats{DiskUsedBytes: c.used, DiskLimitBytes: c.limit}
		if got := s.DiskUsedPct(); got != c.wantPct {
			t.Errorf("%s: DiskUsedPct() = %v, want %v", c.name, got, c.wantPct)
		}
	}
}
