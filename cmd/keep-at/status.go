package main

import (
	"flag"
	"fmt"

	"github.com/tweedge/keep-at/internal/daemonctl"
	"github.com/tweedge/keep-at/internal/netstats"
)

func cmdStatus(args []string) error {
	fs := flag.NewFlagSet("status", flag.ContinueOnError)
	configPath := fs.String("config", "", "path to a config file (optional)")
	if err := fs.Parse(args); err != nil {
		return err
	}

	dir, err := resolveDataDir(*configPath, "")
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
	} else if pid, ok := daemonctl.FindForeground(dir); ok {
		if pid > 0 {
			fmt.Printf("keep-at is running in the foreground (pid %d), not as a service\n", pid)
		} else {
			fmt.Println("keep-at is running in the foreground, not as a service")
		}
	} else {
		fmt.Println("keep-at is not running")
	}

	// The runtime summary is written by the running daemon; if none is
	// present (keep-at hasn't started, or hasn't gotten far enough to write
	// one), just skip the block rather than implying a problem.
	runtimeStats, err := netstats.LoadRuntime(dir + "/runtime-stats.json")
	if err != nil {
		return err
	}
	if runtimeStats.CollectedAt.IsZero() {
		return nil
	}

	seeding := runtimeStats.SeedingTorrents
	downloading := runtimeStats.HeldTorrents - seeding
	if downloading < 0 {
		downloading = 0
	}

	fmt.Printf("runtime stats (as of %s, uptime %s):\n",
		runtimeStats.CollectedAt.Format("2006-01-02 15:04:05 MST"),
		runtimeStats.Uptime().Round(1e9).String())
	fmt.Printf("  torrents: %d held, %d seeding, %d downloading\n", runtimeStats.HeldTorrents, seeding, downloading)
	if runtimeStats.DiskLimitBytes > 0 {
		fmt.Printf("  disk: %s used of %s configured (%.1f%%)\n",
			netstats.HumanBytes(runtimeStats.DiskUsedBytes),
			netstats.HumanBytes(runtimeStats.DiskLimitBytes),
			runtimeStats.DiskUsedPct())
	}
	fmt.Printf("  useful upload since boot: %s (total network %s)\n",
		netstats.HumanBytes(runtimeStats.UsefulBytesUploaded),
		netstats.HumanBytes(runtimeStats.TotalBytesUploaded))
	fmt.Printf("  useful download since boot: %s (total network %s)\n",
		netstats.HumanBytes(runtimeStats.UsefulBytesDownloaded),
		netstats.HumanBytes(runtimeStats.TotalBytesDownloaded))
	fmt.Printf("  avg upload since boot: %s\n", netstats.HumanBitsPerSec(runtimeStats.UploadBitsPerSec()))
	fmt.Printf("  avg download since boot: %s\n", netstats.HumanBitsPerSec(runtimeStats.DownloadBitsPerSec()))
	fmt.Printf("  active peers: %d\n", runtimeStats.ActivePeers)
	return nil
}
