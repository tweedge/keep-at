package main

import (
	"context"
	"flag"
	"fmt"
	"time"

	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/engine"
	"github.com/tweedge/keep-at/internal/netstats"
)

// cmdNetworkStatus runs a full network-status census: it scrapes and probes
// every torrent in the catalog to count other keep-at nodes and how much
// data they're collectively seeding, printing a running status line as it
// goes and a summary at the end.
//
// This is deliberately a separate, synchronous, RAM- and time-heavy
// operation - not something the node itself does. A full-catalog census
// joins every torrent's swarm to count keep-at peers, so it can take hours
// (each probe waits up to the probe timeout) and holds a torrent's worth of
// piece bookkeeping per probe (bounded by the per-torrent probe-client
// lifecycle - see engine.RunCensus). The node itself trusts the tracker's
// seeder counts for selection and never probes swarms during normal
// operation, so this command is how interested parties still get the
// network picture.
func cmdNetworkStatus(args []string) error {
	fs := flag.NewFlagSet("network-status", flag.ContinueOnError)
	configPath := fs.String("config", "", "path to a config file (optional)")
	probeTimeout := fs.Duration("probe-timeout", 10*time.Second, "how long to wait per torrent for peers while probing its swarm")
	dataDir := fs.String("data-dir", "", "directory containing keep-at's cached catalog and metadata (defaults to the config's or OS default)")
	apiKey := fs.String("api-key", "", "Academic Torrents API key to attribute census announces to your account")
	rateLimit := fs.Float64("rate-limit", 0, "max requests per second to Academic Torrents' own infrastructure (0 = default)")
	fs.Usage = func() {
		fmt.Fprint(fs.Output(), `keep-at network-status - census the keep-at network

Scrapes and probes every torrent in the Academic Torrents catalog to count
other keep-at nodes, how much data they're collectively seeding vs still
downloading, and the catalog's p10 seeder floor.

This is RAM- and time-heavy by design: each torrent's swarm is joined to
count keep-at peers (up to --probe-timeout per torrent), so a full-catalog
census can take hours. It is a separate, synchronous operation - the node
itself never does this during normal operation, trusting the tracker for
selection instead.

Usage:
  keep-at network-status [--config PATH] [--probe-timeout DURATION]
                         [--data-dir PATH] [--api-key KEY] [--rate-limit N]

Prints a status line every 25 torrents and a summary when complete.
`)
	}
	if err := fs.Parse(args); err != nil {
		return err
	}

	cf := configFlagSet{
		configPath: configPath,
		dataDir:    dataDir,
		apiKey:     apiKey,
		rateLimit:  rateLimit,
	}
	cfg, err := cf.resolveCensusConfig(fs)
	if err != nil {
		return err
	}

	fmt.Println("keep-at network-status: censusing the keep-at network")
	fmt.Println("NOTE: this is RAM- and time-heavy (joins every torrent's swarm; can take hours).")
	fmt.Printf("data dir: %s\n", cfg.DataDir)
	fmt.Println()

	progress := func(p engine.CensusProgress) {
		pct := 0.0
		if p.Total > 0 {
			pct = float64(p.Processed) / float64(p.Total) * 100
		}
		fmt.Printf("  census in progress: %d/%d torrents (%.1f%%), keep-at nodes observed so far: %d, elapsed %s\n",
			p.Processed, p.Total, pct, p.NodeCount, p.Elapsed.Round(time.Second))
	}

	result, err := engine.RunCensus(context.Background(), cfg, engine.CensusOptions{ProbeTimeout: *probeTimeout}, progress)
	if err != nil {
		return err
	}

	fmt.Println()
	fmt.Printf("census complete: %d/%d torrents scraped and probed (%d failed) in %s\n",
		result.Scraped, result.CatalogSize, result.Failed, result.Elapsed.Round(time.Second))
	fmt.Printf("keep-at nodes observed: %d\n", result.NodeCount)
	fmt.Printf("data being seeded by keep-at nodes: %s\n", netstats.HumanBytes(result.SeedingBytes))
	fmt.Printf("data being downloaded by keep-at nodes: %s\n", netstats.HumanBytes(result.LeechingBytes))
	fmt.Printf("p10 seeder floor (anchor for the seed-scarcity gate): %d\n", result.SeederFloor)
	return nil
}

// resolveCensusConfig builds the config for a census run from the same
// config surface as `run` (data dir, API key, rate limit), without the
// storage-limit requirement - a census stores nothing. Flags are limited to
// what the census needs; everything else comes from the config file (or
// defaults).
func (cf *configFlagSet) resolveCensusConfig(fs *flag.FlagSet) (config.Config, error) {
	visited := map[string]bool{}
	fs.Visit(func(fl *flag.Flag) { visited[fl.Name] = true })

	configPath := *cf.configPath
	if configPath == "" {
		configPath = serviceConfigIfPresent()
	}

	cfg := config.Default()
	if configPath != "" {
		loaded, err := config.Load(configPath)
		if err != nil {
			return config.Config{}, err
		}
		cfg = loaded
	}

	for name := range visited {
		switch name {
		case "data-dir":
			cfg.DataDir = *cf.dataDir
		case "api-key":
			cfg.APIKey = *cf.apiKey
		case "rate-limit":
			cfg.Scan.RateLimitPerSecond = *cf.rateLimit
		}
	}

	return cfg, nil
}
