package main

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/tweedge/mimisbaeti/internal/config"
	"github.com/tweedge/mimisbaeti/internal/daemonctl"
)

func cmdStart(args []string) error {
	fs := flag.NewFlagSet("start", flag.ContinueOnError)
	configPath := fs.String("config", defaultConfigPath(), "path to mimis config file")
	foreground := fs.Bool("foreground", false, "run in the foreground instead of daemonizing")
	if err := fs.Parse(args); err != nil {
		return err
	}

	// Daemonizing (forking into the background and letting this process
	// exit) inside a container just kills the container, since nothing is
	// left running as PID 1. Default to foreground there even if
	// --foreground wasn't passed explicitly.
	if *foreground || daemonctl.IsContainerized() {
		return runForeground(*configPath)
	}

	cfg, err := config.Load(*configPath)
	if err != nil {
		return fmt.Errorf("loading config: %w", err)
	}
	if err := os.MkdirAll(cfg.DataDir, 0o755); err != nil {
		return fmt.Errorf("creating data dir %s: %w", cfg.DataDir, err)
	}

	execPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locating mimis executable: %w", err)
	}

	mgr := daemonManager(cfg)
	if err := mgr.Start(execPath, []string{"run", "--config", *configPath}); err != nil {
		return err
	}

	status, err := mgr.Status()
	if err != nil {
		return err
	}
	fmt.Printf("mimis started (pid %d), logging to %s\n", status.PID, mgr.LogFile)
	return nil
}

func daemonManager(cfg config.Config) daemonctl.Manager {
	return daemonctl.Manager{
		PIDFile: filepath.Join(cfg.DataDir, "mimis.pid"),
		LogFile: filepath.Join(cfg.DataDir, "mimis.log"),
	}
}
