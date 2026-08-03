package main

import (
	"flag"
	"fmt"

	"github.com/tweedge/keep-at/internal/netstats"
)

func cmdNetworkStatus(args []string) error {
	fs := flag.NewFlagSet("network-status", flag.ContinueOnError)
	configPath := fs.String("config", "", "path to a config file (optional)")
	dataDir := fs.String("data-dir", "", "directory keep-at was started with (optional; defaults to the same as `keep-at run`)")
	if err := fs.Parse(args); err != nil {
		return err
	}

	dir, err := resolveDataDir(*configPath, *dataDir)
	if err != nil {
		return err
	}

	snapshot, err := netstats.Load(dir + "/network-stats.json")
	if err != nil {
		return err
	}

	if snapshot.ScanStartedAt.IsZero() {
		fmt.Println("no scan has run yet")
		return nil
	}

	if snapshot.InProgress() {
		fmt.Printf("scan in progress: started %s\n", snapshot.ScanStartedAt.Format("2006-01-02 15:04:05 MST"))
		fmt.Printf("progress: %.0f%% (%d/%d candidates)\n", snapshot.ProgressPercent(), snapshot.ProcessedCandidates, snapshot.TotalCandidates)
	} else {
		fmt.Printf("last scan: %s to %s\n",
			snapshot.ScanStartedAt.Format("2006-01-02 15:04:05 MST"),
			snapshot.ScanCompletedAt.Format("2006-01-02 15:04:05 MST"))
	}

	fmt.Println()
	fmt.Printf("keep-at nodes observed: %d\n", snapshot.NodeCount)
	fmt.Printf("data being seeded by keep-at nodes: %s\n", netstats.HumanBytes(snapshot.SeedingBytes))
	fmt.Printf("data being downloaded by keep-at nodes: %s\n", netstats.HumanBytes(snapshot.LeechingBytes))
	return nil
}
