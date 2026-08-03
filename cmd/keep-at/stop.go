package main

import (
	"flag"
	"fmt"
	"time"
)

func cmdStop(args []string) error {
	fs := flag.NewFlagSet("stop", flag.ContinueOnError)
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
	if err := mgr.Stop(15 * time.Second); err != nil {
		return err
	}
	fmt.Println("keep-at stopped")
	return nil
}
