package main

import (
	"flag"
	"fmt"
)

func cmdStatus(args []string) error {
	fs := flag.NewFlagSet("status", flag.ContinueOnError)
	configPath := fs.String("config", "", "path to a config file (optional)")
	dataDir := fs.String("data-dir", "", "directory keep-at was started with (optional; defaults to the same as `keep-at run`)")
	if err := fs.Parse(args); err != nil {
		return err
	}

	dir, err := resolveDataDir(*configPath, *dataDir)
	if err != nil {
		return err
	}

	mgr := daemonManagerAt(dir)
	status, err := mgr.Status()
	if err != nil {
		return err
	}

	if status.Running {
		fmt.Printf("keep-at is running (pid %d)\n", status.PID)
	} else {
		fmt.Println("keep-at is not running")
	}
	return nil
}
