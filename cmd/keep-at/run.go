package main

import (
	"context"
	"flag"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/engine"
)

func cmdRun(args []string) error {
	fs := flag.NewFlagSet("run", flag.ContinueOnError)
	cf := addConfigFlags(fs)
	if err := fs.Parse(args); err != nil {
		return err
	}

	cfg, err := cf.resolve(fs)
	if err != nil {
		return err
	}

	return runForeground(cfg)
}

// runForeground builds the engine from an already-resolved config and
// blocks running scans until it receives SIGINT or SIGTERM. Both `keep-at
// run` and a daemonized `keep-at start` end up here - the only difference
// is whether something forked this process into the background first.
func runForeground(cfg config.Config) error {
	if err := os.MkdirAll(cfg.DataDir, 0o755); err != nil {
		return fmt.Errorf("creating data dir %s: %w", cfg.DataDir, err)
	}

	level := slog.LevelInfo
	if cfg.Debug {
		level = slog.LevelDebug
	}
	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{Level: level}))

	startedAt := time.Now()
	e, err := engine.New(cfg, engine.Options{Logger: logger})
	if err != nil {
		return fmt.Errorf("starting engine: %w", err)
	}
	defer e.Close()
	startupDuration := time.Since(startedAt)

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	logger.Info("keep-at started", "port", cfg.Port, "data_dir", cfg.DataDir, "startup_time", startupDuration.String())
	return e.Run(ctx)
}
