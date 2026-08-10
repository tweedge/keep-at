package daemonctl

import (
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/tweedge/keep-at/internal/config"
)

// startSleepProcess launches a long-running sleep, so a test has a real
// process to signal and wait on. It reaps the child asynchronously (like
// Manager.Start does) so an exited process isn't left as a zombie, which
// would otherwise keep responding to signal-0 liveness checks.
func startSleepProcess(t *testing.T) *exec.Cmd {
	t.Helper()
	cmd := exec.Command("/bin/sleep", "30")
	if err := cmd.Start(); err != nil {
		t.Fatalf("starting sleep: %v", err)
	}
	go func() { _ = cmd.Wait() }()
	t.Cleanup(func() {
		_ = StopPID(cmd.Process.Pid, 3*time.Second)
	})
	return cmd
}

func TestStartStatusStop(t *testing.T) {
	dir := t.TempDir()
	m := Manager{
		PIDFile: filepath.Join(dir, "keep-at.pid"),
		LogFile: filepath.Join(dir, "keep-at.log"),
	}

	status, err := m.Status()
	if err != nil {
		t.Fatalf("Status (before start): %v", err)
	}
	if status.Running {
		t.Fatalf("expected not running before Start")
	}

	if err := m.Start("/bin/sleep", []string{"30"}); err != nil {
		t.Fatalf("Start: %v", err)
	}

	status, err = m.Status()
	if err != nil {
		t.Fatalf("Status (after start): %v", err)
	}
	if !status.Running {
		t.Fatalf("expected running after Start")
	}

	if err := m.Stop(3 * time.Second); err != nil {
		t.Fatalf("Stop: %v", err)
	}

	status, err = m.Status()
	if err != nil {
		t.Fatalf("Status (after stop): %v", err)
	}
	if status.Running {
		t.Fatalf("expected not running after Stop")
	}
}

// TestStopPIDStopsAnArbitraryProcess exercises the PID-based stop path used
// when keep-at is running in the foreground (no PID file): it must signal
// the given PID and wait for it to exit, exactly like Manager.Stop does for
// the daemonized case.
func TestStopPIDStopsAnArbitraryProcess(t *testing.T) {
	cmd := startSleepProcess(t)

	if err := StopPID(cmd.Process.Pid, 3*time.Second); err != nil {
		t.Fatalf("StopPID: %v", err)
	}

	// Give it a moment to fully exit, then confirm it's gone.
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if !processAlive(cmd.Process.Pid) {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("process %d still alive after StopPID", cmd.Process.Pid)
}

func TestStartRejectsWhenAlreadyRunning(t *testing.T) {
	dir := t.TempDir()
	m := Manager{
		PIDFile: filepath.Join(dir, "keep-at.pid"),
		LogFile: filepath.Join(dir, "keep-at.log"),
	}

	if err := m.Start("/bin/sleep", []string{"30"}); err != nil {
		t.Fatalf("Start: %v", err)
	}
	defer m.Stop(3 * time.Second)

	if err := m.Start("/bin/sleep", []string{"30"}); err == nil {
		t.Fatalf("expected second Start to fail while already running")
	}
}

func TestProcessDataDir(t *testing.T) {
	cases := []struct {
		name string
		args []string
		want func() string
	}{
		{"no flags uses default", []string{"keep-at", "run", "--storage-limit", "10G"}, config.DefaultDataDir},
		{"explicit data dir flag", []string{"keep-at", "run", "--data-dir", "/custom/dir"}, func() string { return "/custom/dir" }},
		{"explicit data dir equals form", []string{"keep-at", "run", "--data-dir=/custom/dir"}, func() string { return "/custom/dir" }},
		{"config file data dir", []string{"keep-at", "run", "--config", "/nonexistent/keep-at.yaml"}, config.DefaultDataDir},
	}
	for _, c := range cases {
		got := processDataDir(c.args)
		if got != c.want() {
			t.Errorf("%s: processDataDir() = %q, want %q", c.name, got, c.want())
		}
	}
}

func TestConfigDataDirFromFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config.yaml")
	cfg := config.Default()
	cfg.DataDir = "/from/config"
	cfg.Storage.Locations = []config.StorageLocation{{Path: filepath.Join(dir, "storage"), Limit: config.ByteSize(1 << 30)}}
	if err := config.Save(path, cfg); err != nil {
		t.Fatalf("Save: %v", err)
	}
	if got := configDataDirFromFile(path); got != "/from/config" {
		t.Errorf("configDataDirFromFile = %q, want /from/config", got)
	}
	if got := configDataDirFromFile("/nonexistent.yaml"); got != config.DefaultDataDir() {
		t.Errorf("missing config should fall back to default, got %q", got)
	}
}

func TestRuntimeStatsFresh(t *testing.T) {
	dir := t.TempDir()
	if runtimeStatsFresh(dir) {
		t.Fatal("no runtime-stats.json should mean not fresh")
	}

	path := filepath.Join(dir, "runtime-stats.json")
	if err := os.WriteFile(path, []byte("{}"), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}
	if !runtimeStatsFresh(dir) {
		t.Fatal("a just-written runtime-stats.json should be fresh")
	}

	old := time.Now().Add(-foregroundFreshWindow - time.Hour)
	if err := os.Chtimes(path, old, old); err != nil {
		t.Fatalf("chtimes: %v", err)
	}
	if runtimeStatsFresh(dir) {
		t.Fatal("a stale runtime-stats.json should not be fresh")
	}
}
