package main

import (
	"flag"
	"fmt"
	"time"

	"github.com/tweedge/mimisbaeti/internal/config"
)

func cmdStop(args []string) error {
	fs := flag.NewFlagSet("stop", flag.ContinueOnError)
	configPath := fs.String("config", defaultConfigPath(), "path to mimis config file")
	if err := fs.Parse(args); err != nil {
		return err
	}

	cfg, err := config.Load(*configPath)
	if err != nil {
		return fmt.Errorf("loading config: %w", err)
	}

	mgr := daemonManager(cfg)
	if err := mgr.Stop(15 * time.Second); err != nil {
		return err
	}
	fmt.Println("mimis stopped")
	return nil
}
