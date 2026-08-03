package main

import (
	"fmt"
	"net/http"
	"os"
	"time"

	"github.com/tweedge/mimisbaeti/internal/buildinfo"
	"github.com/tweedge/mimisbaeti/internal/updater"
)

func cmdSelfUpdate(args []string) error {
	execPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locating mimis executable: %w", err)
	}

	client := &http.Client{Timeout: 60 * time.Second}

	fmt.Printf("current version: %s\n", buildinfo.Version)
	fmt.Println("checking for a newer release...")

	newVersion, err := updater.Apply(client, buildinfo.UserAgent(), execPath)
	if err != nil {
		return fmt.Errorf("self-update: %w", err)
	}

	fmt.Printf("updated to %s\n", newVersion)
	fmt.Println("restart mimis (or `mimis stop && mimis start`) to run the new version")
	return nil
}
