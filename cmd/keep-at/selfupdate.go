package main

import (
	"flag"
	"fmt"
	"net/http"
	"os"
	"time"

	"github.com/tweedge/keep-at/internal/buildinfo"
	"github.com/tweedge/keep-at/internal/updater"
)

func cmdSelfUpdate(args []string) error {
	fs := flag.NewFlagSet("self-update", flag.ContinueOnError)
	beta := fs.Bool("beta", false, "allow updating to a beta (prerelease) version; by default only stable releases are considered")
	if err := fs.Parse(args); err != nil {
		return err
	}

	execPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locating keep-at executable: %w", err)
	}

	client := &http.Client{Timeout: 60 * time.Second}

	fmt.Printf("current version: %s\n", buildinfo.Version)
	if *beta {
		fmt.Println("checking for a newer release (beta versions allowed)...")
	} else {
		fmt.Println("checking for a newer release...")
	}

	newVersion, err := updater.Apply(client, buildinfo.UserAgent(), execPath, *beta)
	if err != nil {
		return fmt.Errorf("self-update: %w", err)
	}

	fmt.Printf("updated to %s\n", newVersion)
	fmt.Println("restart keep-at (or `keep-at stop && keep-at start`) to run the new version")
	return nil
}
