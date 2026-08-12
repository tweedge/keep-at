package engine

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestDebugCollectionWritesMemorySnapshot(t *testing.T) {
	e, _ := newTestEngine(t, 14*24*time.Hour)
	// Rebuild with debug on: the helper doesn't set Debug, so flip it on the
	// existing engine's config. Since Debug only gates the collection loop,
	// mutating the live config is fine for the test.
	e.cfg.Debug = true

	stop := e.startDebugCollection(context.Background())
	defer stop()

	// The collector writes one snapshot immediately on start, so the file
	// must exist without waiting for the ticker.
	path := filepath.Join(e.cfg.DataDir, "debug", "memory.jsonl")
	deadline := time.Now().Add(5 * time.Second)
	for {
		data, err := os.ReadFile(path)
		if err == nil && len(data) > 0 {
			if !strings.Contains(string(data), "process_rss_bytes") {
				t.Fatalf("snapshot line missing expected fields:\n%s", data)
			}
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("debug memory.jsonl was never written to %s", path)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func TestDebugCollectionDisabledIsNoOp(t *testing.T) {
	e, _ := newTestEngine(t, 14*24*time.Hour)
	e.cfg.Debug = false

	stop := e.startDebugCollection(context.Background())
	defer stop()

	if _, err := os.Stat(filepath.Join(e.cfg.DataDir, "debug")); !os.IsNotExist(err) {
		t.Fatal("debug dir should not be created when debug is disabled")
	}
}

func TestAppendLineAndPruneArtifacts(t *testing.T) {
	dir := t.TempDir()

	// appendLine creates the file and appends.
	path := filepath.Join(dir, "memory.jsonl")
	if err := appendLine(path, []byte(`{"a":1}`)); err != nil {
		t.Fatalf("appendLine: %v", err)
	}
	if err := appendLine(path, []byte(`{"a":2}`)); err != nil {
		t.Fatalf("appendLine: %v", err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if got := strings.Count(string(data), "\n"); got != 2 {
		t.Fatalf("expected 2 lines, got %d", got)
	}

	// pruneArtifacts keeps only the newest N matching files.
	for i := 0; i < 5; i++ {
		name := "heap-0000000000000000000" + string(rune('0'+i)) + ".pprof"
		if err := os.WriteFile(filepath.Join(dir, name), []byte("x"), 0o644); err != nil {
			t.Fatalf("write: %v", err)
		}
	}
	pruneArtifacts(dir, "heap-*.pprof", 2)
	remaining, err := filepath.Glob(filepath.Join(dir, "heap-*.pprof"))
	if err != nil {
		t.Fatalf("glob: %v", err)
	}
	if len(remaining) != 2 {
		t.Fatalf("expected 2 heap artifacts kept, got %d", len(remaining))
	}
}

func TestReadProcessRSS(t *testing.T) {
	rss := readProcessRSS()
	if rss <= 0 {
		t.Fatal("expected nonzero RSS from /proc/self/statm")
	}
}
