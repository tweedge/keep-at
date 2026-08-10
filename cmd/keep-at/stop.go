package main

import (
	"flag"
	"fmt"
	"time"

	"github.com/tweedge/keep-at/internal/daemonctl"
)

func cmdStop(args []string) error {
	fs := flag.NewFlagSet("stop", flag.ContinueOnError)
	configPath := fs.String("config", "", "path to a config file (optional)")
	if err := fs.Parse(args); err != nil {
		return err
	}

	dir, err := resolveDataDir(*configPath, "")
	if err != nil {
		return err
	}

	mgr := daemonManagerAt(dir)
	if err := mgr.Stop(15 * time.Second); err != nil {
		// The PID-file path found nothing - but the same can happen when
		// keep-at is running in the foreground (started with `keep-at run`,
		// or as a systemd service, neither of which writes a PID file).
		// status already falls back to scanning the process table for such an
		// instance; do the same here so `keep-at stop` works for every way
		// keep-at can be running, not just the daemonized one.
		pid, ok := daemonctl.FindForeground(dir)
		if !ok || pid <= 0 {
			return err
		}
		if err := daemonctl.StopPID(pid, 15*time.Second); err != nil {
			return err
		}
	}
	fmt.Println("keep-at stopped")
	return nil
}
