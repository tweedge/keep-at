// Package daemonctl implements mimis' default start/stop/status behavior:
// forking itself into the background with a PID file, independent of
// whatever OS service manager (if any) is also managing it. This is what
// makes `mimis start` work the same way on a laptop, a bare Raspberry Pi,
// or inside a container that skips daemonization entirely and just runs in
// the foreground.
package daemonctl

import (
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// Manager owns one mimis process's PID file and log file.
type Manager struct {
	PIDFile string
	LogFile string
}

// Status describes whether the managed process is running.
type Status struct {
	Running bool
	PID     int
}

// Start launches command with args as a detached background process,
// redirecting its output to LogFile, and records its PID. It returns
// immediately; it does not wait for the child to do anything.
func (m Manager) Start(command string, args []string) error {
	if status, err := m.Status(); err == nil && status.Running {
		return fmt.Errorf("daemonctl: already running with pid %d", status.PID)
	}

	logFile, err := os.OpenFile(m.LogFile, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644)
	if err != nil {
		return fmt.Errorf("daemonctl: opening log file %s: %w", m.LogFile, err)
	}
	defer logFile.Close()

	cmd := exec.Command(command, args...)
	cmd.Stdout = logFile
	cmd.Stderr = logFile
	detachFromParent(cmd)

	if err := cmd.Start(); err != nil {
		return fmt.Errorf("daemonctl: starting %s: %w", command, err)
	}

	if err := os.WriteFile(m.PIDFile, []byte(strconv.Itoa(cmd.Process.Pid)), 0o644); err != nil {
		return fmt.Errorf("daemonctl: writing pid file %s: %w", m.PIDFile, err)
	}

	// Detach: let the child outlive this process without becoming a zombie
	// once it exits, since nothing will call Wait on it here.
	go func() { _ = cmd.Wait() }()

	return nil
}

// Stop sends SIGTERM to the managed process and waits briefly for it to
// exit, escalating to SIGKILL if it doesn't.
func (m Manager) Stop(timeout time.Duration) error {
	status, err := m.Status()
	if err != nil {
		return err
	}
	if !status.Running {
		_ = os.Remove(m.PIDFile)
		return fmt.Errorf("daemonctl: not running")
	}

	proc, err := os.FindProcess(status.PID)
	if err != nil {
		return fmt.Errorf("daemonctl: finding process %d: %w", status.PID, err)
	}
	if err := proc.Signal(syscall.SIGTERM); err != nil {
		return fmt.Errorf("daemonctl: signaling process %d: %w", status.PID, err)
	}

	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if !processAlive(status.PID) {
			_ = os.Remove(m.PIDFile)
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}

	if err := proc.Signal(syscall.SIGKILL); err != nil {
		return fmt.Errorf("daemonctl: force-killing process %d: %w", status.PID, err)
	}
	_ = os.Remove(m.PIDFile)
	return nil
}

// Status reports whether the process recorded in PIDFile is actually
// alive, cleaning up a stale PID file if it's not.
func (m Manager) Status() (Status, error) {
	data, err := os.ReadFile(m.PIDFile)
	if os.IsNotExist(err) {
		return Status{Running: false}, nil
	}
	if err != nil {
		return Status{}, fmt.Errorf("daemonctl: reading pid file %s: %w", m.PIDFile, err)
	}

	pid, err := strconv.Atoi(strings.TrimSpace(string(data)))
	if err != nil {
		return Status{}, fmt.Errorf("daemonctl: parsing pid file %s: %w", m.PIDFile, err)
	}

	if !processAlive(pid) {
		_ = os.Remove(m.PIDFile)
		return Status{Running: false}, nil
	}

	return Status{Running: true, PID: pid}, nil
}

func processAlive(pid int) bool {
	proc, err := os.FindProcess(pid)
	if err != nil {
		return false
	}
	// Signal 0 doesn't actually send a signal; it just checks whether the
	// process exists and is signalable by us.
	return proc.Signal(syscall.Signal(0)) == nil
}
