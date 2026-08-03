package main

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/daemonctl"
)

func cmdStart(args []string) error {
	fs := flag.NewFlagSet("start", flag.ContinueOnError)
	foreground := fs.Bool("foreground", false, "run in the foreground instead of daemonizing")
	cf := addConfigFlags(fs)
	if err := fs.Parse(args); err != nil {
		return err
	}

	cfg, err := cf.resolve(fs)
	if err != nil {
		return err
	}

	// Daemonizing (forking into the background and letting this process
	// exit) inside a container just kills the container, since nothing is
	// left running as PID 1. Default to foreground there even if
	// --foreground wasn't passed explicitly.
	if *foreground || daemonctl.IsContainerized() {
		return runForeground(cfg)
	}

	if err := os.MkdirAll(cfg.DataDir, 0o755); err != nil {
		return fmt.Errorf("creating data dir %s: %w", cfg.DataDir, err)
	}

	execPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locating keep-at executable: %w", err)
	}

	// Forward every flag this invocation understood except --foreground,
	// which "run" doesn't take, straight through to the daemonized child.
	// This is what lets `start` support the exact same flags as `run`
	// without duplicating the config-resolution logic across processes.
	childArgs := append([]string{"run"}, argsWithoutForeground(args)...)

	mgr := daemonManagerAt(cfg.DataDir)
	if err := mgr.Start(execPath, childArgs); err != nil {
		return err
	}

	status, err := mgr.Status()
	if err != nil {
		return err
	}
	fmt.Printf("keep-at started (pid %d), logging to %s\n", status.PID, mgr.LogFile)
	return nil
}

func argsWithoutForeground(args []string) []string {
	out := make([]string, 0, len(args))
	for _, a := range args {
		if a == "--foreground" || a == "-foreground" || a == "--foreground=true" || a == "-foreground=true" {
			continue
		}
		out = append(out, a)
	}
	return out
}

func daemonManagerAt(dataDir string) daemonctl.Manager {
	return daemonctl.Manager{
		PIDFile: filepath.Join(dataDir, "keep-at.pid"),
		LogFile: filepath.Join(dataDir, "keep-at.log"),
	}
}

// resolveDataDir figures out where keep-at's PID/log files live without
// requiring the full config (storage location and limit) that `run` and
// `start` need - stop/status/network-status just need to find the same
// data_dir a running instance was started with. If neither configPath nor
// dataDir is given, it checks for an installed service's config
// (service.ConfigPath) before falling back to the plain OS default, so
// these commands work with no arguments at all once keep-at is installed
// as a service.
func resolveDataDir(configPath, dataDir string) (string, error) {
	if configPath == "" {
		configPath = serviceConfigIfPresent()
	}
	if configPath != "" {
		cfg, err := config.Load(configPath)
		if err != nil {
			return "", err
		}
		return cfg.DataDir, nil
	}
	if dataDir != "" {
		return dataDir, nil
	}
	return config.DefaultDataDir(), nil
}
