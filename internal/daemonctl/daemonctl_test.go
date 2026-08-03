package daemonctl

import (
	"path/filepath"
	"testing"
	"time"
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
