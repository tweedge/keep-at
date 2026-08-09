package daemonctl

import (
	"path/filepath"
	"testing"
	"time"

	"github.com/tweedge/keep-at/internal/config"
)

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
