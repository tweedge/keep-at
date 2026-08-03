package main

import (
	"context"
	"flag"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"github.com/tweedge/mimisbaeti/internal/config"
	"github.com/tweedge/mimisbaeti/internal/engine"
)

func cmdRun(args []string) error {
	fs := flag.NewFlagSet("run", flag.ContinueOnError)
	configPath := fs.String("config", defaultConfigPath(), "path to mimis config file")
	if err := fs.Parse(args); err != nil {
		return err
	}

	return runForeground(*configPath)
}

// runForeground loads config, builds the engine, and blocks running scans
// until it receives SIGINT or SIGTERM. Both `mimis run` and a daemonized
// `mimis start` end up here - the only difference is whether something
// forked this process into the background first.
func runForeground(configPath string) error {
	cfg, err := config.Load(configPath)
	if err != nil {
		return fmt.Errorf("loading config: %w", err)
	}

	if err := os.MkdirAll(cfg.DataDir, 0o755); err != nil {
		return fmt.Errorf("creating data dir %s: %w", cfg.DataDir, err)
	}

	logger := slog.New(slog.NewTextHandler(os.Stdout, nil))

	e, err := engine.New(cfg, engine.Options{Logger: logger})
	if err != nil {
		return fmt.Errorf("starting engine: %w", err)
	}
	defer e.Close()

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	logger.Info("mimis started", "config", configPath, "port", cfg.Port)
	return e.Run(ctx)
}
