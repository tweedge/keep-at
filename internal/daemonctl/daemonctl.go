// Package daemonctl implements keep-at's default start/stop/status behavior:
// forking itself into the background with a PID file, independent of
// whatever OS service manager (if any) is also managing it. This is what
// makes `keep-at start` work the same way on a laptop, a bare Raspberry Pi,
// or inside a container that skips daemonization entirely and just runs in
// the foreground.
package daemonctl

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/tweedge/keep-at/internal/config"
)

// Manager owns one keep-at process's PID file and log file.
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

// FindForeground scans the process table for a keep-at instance that was
// started with `keep-at run` in the foreground rather than daemonized with a
// PID file, and that uses dataDir. It returns its PID and true if one is
// running. This is what lets `keep-at status` report a foreground instance
// as running instead of wrongly concluding keep-at isn't running at all just
// because there's no PID file.
//
// A foreground instance is identified as a process running the keep-at
// binary (matched by executable basename, so a differently-named or -copied
// install still counts) whose command line has `run` as the subcommand and
// whose effective data dir - from its --data-dir/--config flags, or the
// default if neither is given - matches dataDir. A daemonized service also
// runs `keep-at run` internally, but in that case the PID file exists and
// Status already reports it, so FindForeground is only consulted when there
// is no (alive) PID file - i.e. the instance was genuinely started in the
// foreground.
//
// On platforms without /proc (macOS, Windows) the process scan finds nothing,
// so the check falls back to a portable liveness signal: the engine writes
// runtime-stats.json into dataDir at startup and every stats_interval, so a
// recently-modified file means a keep-at is running there. The PID is
// unknown in that case (reported as 0).
func FindForeground(dataDir string) (int, bool) {
	if pid, ok := findForegroundProc(dataDir); ok {
		return pid, true
	}
	if runtimeStatsFresh(dataDir) {
		return 0, true
	}
	return 0, false
}

// findForegroundProc is the /proc-based process scan; it's a no-op (returns
// not-found) on platforms where /proc doesn't exist.
func findForegroundProc(dataDir string) (int, bool) {
	selfPid := os.Getpid()

	procs, err := os.ReadDir("/proc")
	if err != nil {
		return 0, false
	}
	for _, proc := range procs {
		if !proc.IsDir() {
			continue
		}
		pid, err := strconv.Atoi(proc.Name())
		if err != nil || pid == selfPid {
			continue
		}

		// Running the keep-at binary (basename match).
		exe, err := os.Readlink(filepath.Join("/proc", proc.Name(), "exe"))
		if err != nil {
			continue
		}
		if filepath.Base(exe) != "keep-at" {
			continue
		}

		// `run` subcommand, not `status`/`stop`/etc.
		cmdline, err := os.ReadFile(filepath.Join("/proc", proc.Name(), "cmdline"))
		if err != nil {
			continue
		}
		args := strings.Split(strings.TrimRight(string(cmdline), "\x00"), "\x00")
		if len(args) < 2 || args[1] != "run" {
			continue
		}

		if processDataDir(args) == dataDir {
			return pid, true
		}
	}
	return 0, false
}

// foregroundFreshWindow is how old runtime-stats.json may be before keep-at
// is considered to have stopped. The engine writes it at startup and every
// stats_interval (default 30 minutes), so a window comfortably larger than
// that (plus one missed write) is enough to confirm liveness without
// false-positiving on a file from a long-gone process.
const foregroundFreshWindow = 2 * time.Hour

// runtimeStatsFresh reports whether keep-at has written its runtime stats
// into dataDir recently enough to be considered running. This is the
// portable liveness check used on platforms without /proc.
func runtimeStatsFresh(dataDir string) bool {
	info, err := os.Stat(filepath.Join(dataDir, "runtime-stats.json"))
	if err != nil {
		return false
	}
	return time.Since(info.ModTime()) < foregroundFreshWindow
}

// processDataDir figures out which data dir a `keep-at run` process uses
// from its command line: the --data-dir flag if given, otherwise the data
// dir implied by --config, otherwise the default.
func processDataDir(args []string) string {
	for i := 0; i < len(args); i++ {
		switch {
		case args[i] == "--data-dir" && i+1 < len(args):
			return args[i+1]
		case strings.HasPrefix(args[i], "--data-dir="):
			return strings.TrimPrefix(args[i], "--data-dir=")
		case args[i] == "--config" && i+1 < len(args):
			return configDataDirFromFile(args[i+1])
		case strings.HasPrefix(args[i], "--config="):
			return configDataDirFromFile(strings.TrimPrefix(args[i], "--config="))
		}
	}
	return config.DefaultDataDir()
}

// configDataDirFromFile returns the data dir a config file sets, or the
// default if the file can't be read or doesn't specify one. Best effort: a
// foreground instance identified this way still needs its data dir to match
// the caller's, so a wrong guess just means status falls through to "not
// running" rather than misreporting a different instance.
func configDataDirFromFile(path string) string {
	cfg, err := config.Load(path)
	if err != nil {
		return config.DefaultDataDir()
	}
	return cfg.DataDir
}
