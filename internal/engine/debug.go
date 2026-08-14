package engine

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"runtime/pprof"
	"runtime/trace"
	"sync"
	"time"
)

// cpuProfileMu serializes CPU profile captures: pprof.StartCPUProfile fails
// if one is already running, and the profile ticker can in principle fire
// while a capture is still in flight if a capture runs long.
var cpuProfileMu sync.Mutex

// debugCollectInterval is how often the debug collector writes a memory
// snapshot (MemStats + process RSS) to <data_dir>/debug/memory.jsonl. A
// fast-moving, low-footprint sample cadence lets a remote operator's hours
// of debug data be turned into a memory-over-time curve, which is the first
// thing that identifies a slow leak, a scan-induced spike, or an OOM ramp.
const debugCollectInterval = 60 * time.Second

// debugProfileInterval is how often the debug collector captures the
// heavier artifacts: a heap pprof, a goroutine dump, and a CPU trace. These
// are what pin down *where* memory is held (the heap profile is the
// definitive "who owns the RAM" answer), so they're captured on a cadence
// that brackets a typical multi-hour failure window without the profiles
// themselves becoming a disk problem.
const debugProfileInterval = 15 * time.Minute

// debugTraceDuration is how long each CPU trace capture runs. Traces are
// the heaviest artifact - a few seconds of trace at this interval keeps the
// debug daemon's own footprint negligible while still catching a busy scan
// or a stuck loop.
const debugTraceDuration = 5 * time.Second

// debugArtifactsKept is how many of each rotated artifact (heap, goroutine
// dump, trace) are kept on disk before the oldest is pruned, so a debug run
// left on for weeks doesn't fill the data dir.
const debugArtifactsKept = 20

// startDebugCollection launches the debug collector if cfg.Debug is set, and
// returns a stop function. It's safe to call regardless of the setting (it
// returns a no-op when debug is off). The collector writes everything under
// <data_dir>/debug/:
//
//   - memory.jsonl: one JSON line per debugCollectInterval with Go runtime
//     memory stats (heap alloc, heap sys, total alloc, GC cycle counts and
//     pauses) plus the process's resident set size from the OS - the
//     continuous low-footprint record.
//   - heap-<timestamp>.pprof: heap profile, the primary "what holds the
//     RAM" artifact. Read with `go tool pprof`.
//   - goroutines-<timestamp>.txt: full goroutine stack dump, for finding
//     stuck loops and leaked goroutines.
//   - trace-<timestamp>.out: a short CPU trace, for CPU-bound scans.
//
// The goal is a directory a remote operator can tar up after a few hours
// (or however long the failure takes to recur) and hand over for analysis,
// rather than needing interactive access to the host.
func (e *Engine) startDebugCollection(ctx context.Context) func() {
	if !e.cfg.Debug {
		return func() {}
	}

	dir := filepath.Join(e.cfg.DataDir, "debug")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		e.logger.Warn("debug collection enabled but could not create debug dir", "dir", dir, "err", err)
		return func() {}
	}

	e.logger.Info("debug collection enabled; writing diagnostics to " + dir)
	e.writeMemorySnapshot(dir) // capture one immediately so a short run still has data

	stop := make(chan struct{})
	go func() {
		collectTicker := time.NewTicker(debugCollectInterval)
		defer collectTicker.Stop()
		profileTicker := time.NewTicker(debugProfileInterval)
		defer profileTicker.Stop()

		for {
			select {
			case <-stop:
				return
			case <-ctx.Done():
				return
			case <-collectTicker.C:
				e.writeMemorySnapshot(dir)
			case <-profileTicker.C:
				e.captureDebugProfiles(dir)
			}
		}
	}()

	return func() {
		close(stop)
		e.writeMemorySnapshot(dir) // final snapshot so shutdown state is captured
	}
}

// memorySnapshot is one line of debug/memory.jsonl.
type memorySnapshot struct {
	Time string `json:"time"`

	// Process RSS in bytes, from the OS (/proc/self/statm on Linux, 0 when
	// it can't be read). This is the number that actually matters for OOM:
	// Go's heap-in-use can be well under the resident set size because the
	// runtime keeps freed pages and grows the arena lazily.
	ProcessRSSBytes int64 `json:"process_rss_bytes"`

	// Go runtime heap figures.
	HeapAllocBytes uint64 `json:"heap_alloc_bytes"`
	HeapSysBytes   uint64 `json:"heap_sys_bytes"`
	HeapIdleBytes  uint64 `json:"heap_idle_bytes"`
	HeapInuseBytes uint64 `json:"heap_inuse_bytes"`

	// Cumulative allocations and live objects.
	TotalAllocBytes uint64 `json:"total_alloc_bytes"`
	LiveObjects     uint64 `json:"live_objects"`

	// Goroutines and GC activity.
	Goroutines       int     `json:"goroutines"`
	NumGC            uint32  `json:"num_gc"`
	LastGCPauseMicro int64   `json:"last_gc_pause_us"`
	GCPauseAvgMicro  float64 `json:"gc_pause_avg_us"`
	GCPauseTotalSec  float64 `json:"gc_pause_total_sec"`

	// How many torrents the engine is holding and seeding, so the memory
	// curve can be correlated with what keep-at was actually doing.
	HeldTorrents    int `json:"held_torrents"`
	SeedingTorrents int `json:"seeding_torrents"`
	ActivePeers     int `json:"active_peers"`
}

// writeMemorySnapshot appends one memorySnapshot line to debug/memory.jsonl
// and also logs it at debug level. It's called on a ticker, once at startup,
// and once at shutdown.
func (e *Engine) writeMemorySnapshot(dir string) {
	rss := readProcessRSS()

	var ms runtime.MemStats
	runtime.ReadMemStats(&ms)

	held := e.state.All()

	seeding := 0
	for _, t := range e.torrentClient.Torrents() {
		if t.Seeding() {
			seeding++
		}
	}

	stats := e.torrentClient.Stats()

	snap := memorySnapshot{
		Time:             time.Now().UTC().Format(time.RFC3339),
		ProcessRSSBytes:  rss,
		HeapAllocBytes:   ms.HeapAlloc,
		HeapSysBytes:     ms.HeapSys,
		HeapIdleBytes:    ms.HeapIdle,
		HeapInuseBytes:   ms.HeapInuse,
		TotalAllocBytes:  ms.TotalAlloc,
		LiveObjects:      ms.Mallocs - ms.Frees,
		Goroutines:       runtime.NumGoroutine(),
		NumGC:            ms.NumGC,
		LastGCPauseMicro: int64(ms.PauseNs[(ms.NumGC+255)%256]) / 1000,
		GCPauseTotalSec:  float64(ms.PauseTotalNs) / 1e9,
		HeldTorrents:     len(held),
		SeedingTorrents:  seeding,
		ActivePeers:      stats.TotalPeers,
	}
	if ms.NumGC > 0 {
		snap.GCPauseAvgMicro = float64(ms.PauseTotalNs) / float64(ms.NumGC) / 1000.0
	}

	line, err := json.Marshal(snap)
	if err != nil {
		e.logger.Warn("debug: could not marshal memory snapshot", "err", err)
		return
	}

	path := filepath.Join(dir, "memory.jsonl")
	if err := appendLine(path, line); err != nil {
		e.logger.Warn("debug: could not write memory snapshot", "path", path, "err", err)
		return
	}

	e.logger.Debug("debug memory snapshot",
		"rss", humanBytes(snap.ProcessRSSBytes),
		"heap_alloc", humanBytes(int64(snap.HeapAllocBytes)),
		"heap_sys", humanBytes(int64(snap.HeapSysBytes)),
		"goroutines", snap.Goroutines,
		"gc_pauses_total", snap.GCPauseTotalSec,
		"held", snap.HeldTorrents,
		"seeding", snap.SeedingTorrents,
		"peers", snap.ActivePeers)
}

// captureDebugProfiles writes a heap profile, a goroutine dump, a CPU
// profile, and a short CPU trace into dir, pruning old artifacts to
// debugArtifactsKept each. The heap profile is the definitive answer to
// "what is holding this RAM"; the CPU profile to "what is burning this
// CPU" (the two questions that dominate triage on remote hosts); the
// goroutine dump catches stuck loops and leaks; the trace catches
// CPU-bound work.
func (e *Engine) captureDebugProfiles(dir string) {
	now := time.Now().UTC()
	stamp := now.Format("20060102T150405Z")

	// Heap profile. This is a moment-in-time picture of who owns the live
	// heap, which is what distinguishes "a legitimately held torrent set"
	// from "memory that should have been freed".
	if err := writePprofProfile(dir, "heap-"+stamp+".pprof", "heap"); err != nil {
		e.logger.Warn("debug: heap profile failed", "err", err)
	}

	// CPU profile: a short sampling profile of where the process is
	// actually spending time. Sampled (not traced) so it can be read with
	// `go tool pprof` the same way as the heap profile - the default
	// `go tool pprof cpu-*.pprof` view is exactly the "why is CPU pegged"
	// answer for a host that's burning cycles without moving data.
	if err := writeCPUProfile(dir, "cpu-"+stamp+".pprof", debugTraceDuration); err != nil {
		e.logger.Warn("debug: cpu profile failed", "err", err)
	}

	// Full goroutine dump, for stuck/leaked goroutines.
	if err := writeGoroutineDump(dir, "goroutines-"+stamp+".txt"); err != nil {
		e.logger.Warn("debug: goroutine dump failed", "err", err)
	}

	// Short CPU trace for CPU-bound scans. Traces are the most invasive
	// artifact (they halt the world while writing), so they're deliberately
	// short and infrequent.
	if err := writeCPUTrace(dir, "trace-"+stamp+".out", debugTraceDuration); err != nil {
		e.logger.Warn("debug: cpu trace failed", "err", err)
	}

	pruneArtifacts(dir, "heap-*.pprof", debugArtifactsKept)
	pruneArtifacts(dir, "cpu-*.pprof", debugArtifactsKept)
	pruneArtifacts(dir, "goroutines-*.txt", debugArtifactsKept)
	pruneArtifacts(dir, "trace-*.out", debugArtifactsKept)
}

// writeCPUProfile samples the process's CPU for duration and writes a pprof
// CPU profile to dir/name. CPU profiling must not overlap itself, so it's
// serialized through a mutex (a capture that overlaps a previous one would
// fail; this just skips the new capture instead).
func writeCPUProfile(dir, name string, duration time.Duration) error {
	cpuProfileMu.Lock()
	defer cpuProfileMu.Unlock()

	f, err := os.Create(filepath.Join(dir, name))
	if err != nil {
		return err
	}
	if err := pprof.StartCPUProfile(f); err != nil {
		f.Close()
		return err
	}
	time.Sleep(duration)
	pprof.StopCPUProfile()
	return f.Close()
}

func writePprofProfile(dir, name, profileName string) error {
	f, err := os.Create(filepath.Join(dir, name))
	if err != nil {
		return err
	}
	defer f.Close()
	return pprof.Lookup(profileName).WriteTo(f, 0)
}

func writeGoroutineDump(dir, name string) error {
	buf := make([]byte, 1<<20)
	n := runtime.Stack(buf, true)
	for n == len(buf) {
		buf = make([]byte, len(buf)*2)
		n = runtime.Stack(buf, true)
	}
	return os.WriteFile(filepath.Join(dir, name), buf[:n], 0o644)
}

func writeCPUTrace(dir, name string, duration time.Duration) error {
	f, err := os.Create(filepath.Join(dir, name))
	if err != nil {
		return err
	}
	defer f.Close()
	if err := trace.Start(f); err != nil {
		return err
	}
	time.Sleep(duration)
	trace.Stop()
	return nil
}

// pruneArtifacts keeps only the newest debugArtifactsKept files matching
// glob, deleting older ones so a long-running debug session doesn't fill the
// data dir.
func pruneArtifacts(dir, glob string, keep int) {
	matches, err := filepath.Glob(filepath.Join(dir, glob))
	if err != nil {
		return
	}
	// Glob returns sorted paths, which for these timestamped names is
	// chronological order. Keep the newest `keep`.
	for _, path := range matches[:maxInt(0, len(matches)-keep)] {
		_ = os.Remove(path)
	}
}

func maxInt(a, b int) int {
	if a > b {
		return a
	}
	return b
}

// appendLine appends one line to path, creating the file if needed.
func appendLine(path string, line []byte) error {
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	defer f.Close()
	_, err = f.Write(append(line, '\n'))
	return err
}

// readProcessRSS returns the process's resident set size in bytes by reading
// /proc/self/statm (Linux). It returns 0 on other platforms or any error -
// the snapshot just omits the RSS figure rather than failing the whole
// collection. statm's second field is RSS in pages.
func readProcessRSS() int64 {
	data, err := os.ReadFile("/proc/self/statm")
	if err != nil {
		return 0
	}
	var size, rss uint64
	if _, err := fmt.Sscanf(string(data), "%d %d", &size, &rss); err != nil {
		return 0
	}
	return int64(rss) * int64(os.Getpagesize())
}

// DebugInfo returns a short description of what keep-at is doing, for
// triage. It's only called when debug logging is enabled so its cost (a
// full state snapshot) is never paid in normal operation.
func (e *Engine) DebugInfo() map[string]any {
	held := e.state.All()
	seeding := 0
	for _, t := range e.torrentClient.Torrents() {
		if t.Seeding() {
			seeding++
		}
	}
	return map[string]any{
		"held_torrents":    len(held),
		"seeding_torrents": seeding,
		"max_torrents":     e.maxTorrents,
		"goroutines":       runtime.NumGoroutine(),
		"debug_dir":        filepath.Join(e.cfg.DataDir, "debug"),
	}
}
