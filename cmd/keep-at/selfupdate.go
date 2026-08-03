package main

import (
	"fmt"
	"net/http"
	"os"
	"time"

	"github.com/tweedge/keep-at/internal/buildinfo"
	"github.com/tweedge/keep-at/internal/updater"
)

func cmdSelfUpdate(args []string) error {
	execPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locating keep-at executable: %w", err)
	}

	client := &http.Client{Timeout: 60 * time.Second}

	fmt.Printf("current version: %s\n", buildinfo.Version)
	fmt.Println("checking for a newer release...")

	newVersion, err := updater.Apply(client, buildinfo.UserAgent(), execPath)
	if err != nil {
		return fmt.Errorf("self-update: %w", err)
	}

	fmt.Printf("updated to %s\n", newVersion)
	fmt.Println("restart keep-at (or `keep-at stop && keep-at start`) to run the new version")
	return nil
}
