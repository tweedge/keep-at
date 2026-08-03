package main

import (
	"os"
	"path/filepath"
)

// defaultConfigPath is where mimis looks for its config file if --config
// isn't given. MIMIS_CONFIG overrides this outright (used by the Docker
// image); otherwise it follows the same XDG-ish convention as config.
// Default's data directory.
func defaultConfigPath() string {
	if fromEnv := os.Getenv("MIMIS_CONFIG"); fromEnv != "" {
		return fromEnv
	}
	if dir, err := os.UserConfigDir(); err == nil && dir != "" {
		return filepath.Join(dir, "mimisbaeti", "config.yaml")
	}
	return "mimisbaeti.yaml"
}
