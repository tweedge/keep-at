package main

import (
	"flag"
	"path/filepath"
	"testing"

	"github.com/tweedge/keep-at/internal/config"
)

// withServiceConfigPath points the package's service-config lookup at a
// temp path for the duration of a test, restoring the real path
// afterward.
func withServiceConfigPath(t *testing.T, path string) {
	t.Helper()
	orig := serviceConfigPath
	serviceConfigPath = path
	t.Cleanup(func() { serviceConfigPath = orig })
}

func TestResolveWithStorageLimitFlagOnly(t *testing.T) {
	withServiceConfigPath(t, filepath.Join(t.TempDir(), "nonexistent.yaml"))

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cf := addConfigFlags(fs)
	if err := fs.Parse([]string{"--storage-limit", "500G"}); err != nil {
		t.Fatalf("Parse: %v", err)
	}

	cfg, err := cf.resolve(fs)
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if len(cfg.Storage.Locations) != 1 || cfg.Storage.Locations[0].Limit.String() != "500G" {
		t.Fatalf("unexpected storage locations: %+v", cfg.Storage.Locations)
	}
}

func TestResolveWithStorageLimitAllFlag(t *testing.T) {
	withServiceConfigPath(t, filepath.Join(t.TempDir(), "nonexistent.yaml"))

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cf := addConfigFlags(fs)
	if err := fs.Parse([]string{"--storage-limit", "all"}); err != nil {
		t.Fatalf("Parse: %v", err)
	}

	cfg, err := cf.resolve(fs)
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if len(cfg.Storage.Locations) != 1 {
		t.Fatalf("expected 1 storage location, got %+v", cfg.Storage.Locations)
	}
	loc := cfg.Storage.Locations[0]
	if !loc.LimitAll {
		t.Fatalf("expected LimitAll for `--storage-limit all`, got %+v", loc)
	}
	if err := cfg.Validate(); err != nil {
		t.Fatalf("expected `all` to validate, got: %v", err)
	}
}

func TestResolveWithStallEvictionTimeoutFlag(t *testing.T) {
	withServiceConfigPath(t, filepath.Join(t.TempDir(), "nonexistent.yaml"))

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cf := addConfigFlags(fs)
	if err := fs.Parse([]string{"--storage-limit", "500G", "--stall-eviction-timeout", "720h"}); err != nil {
		t.Fatalf("Parse: %v", err)
	}

	cfg, err := cf.resolve(fs)
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if got := cfg.Scan.StallEvictionTimeout.AsDuration().Hours(); got != 720 {
		t.Fatalf("StallEvictionTimeout = %v hours, want 720", got)
	}
}

func TestResolveErrorsWithNoFlagsAndNoServiceConfig(t *testing.T) {
	withServiceConfigPath(t, filepath.Join(t.TempDir(), "nonexistent.yaml"))

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cf := addConfigFlags(fs)
	if err := fs.Parse(nil); err != nil {
		t.Fatalf("Parse: %v", err)
	}

	if _, err := cf.resolve(fs); err == nil {
		t.Fatalf("expected an error when nothing is configured")
	}
}

func TestResolveFallsBackToInstalledServiceConfig(t *testing.T) {
	svcConfigPath := filepath.Join(t.TempDir(), "config.yaml")
	withServiceConfigPath(t, svcConfigPath)

	installed := config.Default()
	installed.Port = 55555
	installed.Storage.Locations = []config.StorageLocation{{Path: "/srv/keep-at", Limit: config.ByteSize(1 << 40)}}
	if err := config.Save(svcConfigPath, installed); err != nil {
		t.Fatalf("Save: %v", err)
	}

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cf := addConfigFlags(fs)
	if err := fs.Parse(nil); err != nil {
		t.Fatalf("Parse: %v", err)
	}

	cfg, err := cf.resolve(fs)
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if cfg.Port != 55555 {
		t.Fatalf("expected port from installed service config, got %d", cfg.Port)
	}
	if len(cfg.Storage.Locations) != 1 || cfg.Storage.Locations[0].Path != "/srv/keep-at" {
		t.Fatalf("unexpected storage locations: %+v", cfg.Storage.Locations)
	}
}

func TestResolveOverridesInstalledServiceConfigWithExplicitFlag(t *testing.T) {
	svcConfigPath := filepath.Join(t.TempDir(), "config.yaml")
	withServiceConfigPath(t, svcConfigPath)

	installed := config.Default()
	installed.Port = 55555
	installed.Storage.Locations = []config.StorageLocation{{Path: "/srv/keep-at", Limit: config.ByteSize(1 << 40)}}
	if err := config.Save(svcConfigPath, installed); err != nil {
		t.Fatalf("Save: %v", err)
	}

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cf := addConfigFlags(fs)
	if err := fs.Parse([]string{"--port", "9999"}); err != nil {
		t.Fatalf("Parse: %v", err)
	}

	cfg, err := cf.resolve(fs)
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if cfg.Port != 9999 {
		t.Fatalf("expected explicit --port to override installed service config, got %d", cfg.Port)
	}
	// Storage should still come from the service config since --storage
	// wasn't passed.
	if len(cfg.Storage.Locations) != 1 || cfg.Storage.Locations[0].Path != "/srv/keep-at" {
		t.Fatalf("unexpected storage locations: %+v", cfg.Storage.Locations)
	}
}

func TestResolveRejectsStorageFlagsCombinedWithConfig(t *testing.T) {
	withServiceConfigPath(t, filepath.Join(t.TempDir(), "nonexistent.yaml"))

	configPath := filepath.Join(t.TempDir(), "explicit.yaml")
	explicit := config.Default()
	explicit.Storage.Locations = []config.StorageLocation{{Path: "/data", Limit: config.ByteSize(1 << 40)}}
	if err := config.Save(configPath, explicit); err != nil {
		t.Fatalf("Save: %v", err)
	}

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cf := addConfigFlags(fs)
	if err := fs.Parse([]string{"--config", configPath, "--storage-limit", "500G"}); err != nil {
		t.Fatalf("Parse: %v", err)
	}

	if _, err := cf.resolve(fs); err == nil {
		t.Fatalf("expected an error combining --config with --storage-limit")
	}
}

func TestResolveDataDirFallsBackToInstalledServiceConfig(t *testing.T) {
	svcConfigPath := filepath.Join(t.TempDir(), "config.yaml")
	withServiceConfigPath(t, svcConfigPath)

	installed := config.Default()
	installed.DataDir = "/var/lib/keep-at-service"
	installed.Storage.Locations = []config.StorageLocation{{Path: "/srv/keep-at", Limit: config.ByteSize(1 << 40)}}
	if err := config.Save(svcConfigPath, installed); err != nil {
		t.Fatalf("Save: %v", err)
	}

	dir, err := resolveDataDir("", "")
	if err != nil {
		t.Fatalf("resolveDataDir: %v", err)
	}
	if dir != "/var/lib/keep-at-service" {
		t.Fatalf("resolveDataDir() = %q, want /var/lib/keep-at-service", dir)
	}
}
