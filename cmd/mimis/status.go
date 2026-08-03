package main

import (
	"flag"
	"fmt"

	"github.com/tweedge/mimisbaeti/internal/config"
)

func cmdStatus(args []string) error {
	fs := flag.NewFlagSet("status", flag.ContinueOnError)
	configPath := fs.String("config", defaultConfigPath(), "path to mimis config file")
	if err := fs.Parse(args); err != nil {
		return err
	}

	cfg, err := config.Load(*configPath)
	if err != nil {
		return fmt.Errorf("loading config: %w", err)
	}

	mgr := daemonManager(cfg)
	status, err := mgr.Status()
	if err != nil {
		return err
	}

	if status.Running {
		fmt.Printf("mimis is running (pid %d)\n", status.PID)
	} else {
		fmt.Println("mimis is not running")
	}
	return nil
}
