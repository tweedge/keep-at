package main

import (
	"flag"
	"fmt"
	"os"
	"strconv"

	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/service"
)

func cmdService(args []string) error {
	if len(args) < 1 {
		return fmt.Errorf("usage: keep-at service <install|uninstall>")
	}

	switch args[0] {
	case "install":
		return cmdServiceInstall(args[1:])
	case "uninstall":
		return service.Uninstall()
	default:
		return fmt.Errorf("usage: keep-at service <install|uninstall>")
	}
}

// cmdServiceInstall takes the same flags as `run`, resolves and validates
// them up front (so a bad flag fails now, not after the service is
// installed and systemd is silently restart-looping it), then bakes the
// equivalent explicit flags into the systemd unit's ExecStart line. This
// keeps a config file optional for services too: the unit is
// self-contained, not dependent on flags remembered from install time.
func cmdServiceInstall(args []string) error {
	fs := flag.NewFlagSet("service install", flag.ContinueOnError)
	user := fs.String("user", "root", "user the systemd service runs as")
	cf := addConfigFlags(fs)
	if err := fs.Parse(args); err != nil {
		return err
	}

	cfg, err := cf.resolve(fs)
	if err != nil {
		return err
	}

	execPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locating keep-at executable: %w", err)
	}

	if err := service.Install(service.InstallOpts{
		ExecPath: execPath,
		RunArgs:  configToRunArgs(cfg, *cf.configPath),
		User:     *user,
	}); err != nil {
		return err
	}

	fmt.Println("keep-at service installed and started")
	return nil
}

// configToRunArgs reconstructs the `run` flags that reproduce cfg, so the
// systemd unit is self-contained rather than depending on flags
// remembered from install time.
//
// If a config file was used at install time, the unit just references
// that same file path: multi-location storage setups only exist in config
// files (see configFlagSet.resolve), and re-reading the file on every
// service start means editing it (e.g. adding a disk) takes effect on the
// next restart without reinstalling the service.
func configToRunArgs(cfg config.Config, configPath string) []string {
	if configPath != "" {
		return []string{"run", "--config", configPath}
	}

	args := []string{
		"run",
		"--port", strconv.Itoa(cfg.Port),
		"--data-dir", cfg.DataDir,
		"--aggressiveness", strconv.FormatFloat(cfg.Aggressiveness, 'g', -1, 64),
		"--min-seed-margin", strconv.Itoa(cfg.Scan.MinSeedMargin),
		"--scan-interval", cfg.Scan.Interval.AsDuration().String(),
		"--moderation-delay", cfg.Scan.ModerationDelay.AsDuration().String(),
		"--rate-limit", strconv.FormatFloat(cfg.Scan.RateLimitPerSecond, 'g', -1, 64),
	}
	if len(cfg.Storage.Locations) == 1 {
		loc := cfg.Storage.Locations[0]
		args = append(args, "--storage", loc.Path, "--storage-limit", loc.Limit.String())
	}
	if len(cfg.KeywordBlocklist) > 0 {
		joined := ""
		for i, kw := range cfg.KeywordBlocklist {
			if i > 0 {
				joined += ","
			}
			joined += kw
		}
		args = append(args, "--keyword-blocklist", joined)
	}
	if cfg.PreserveDeletedTorrents {
		args = append(args, "--preserve-deleted-torrents")
	}
	return args
}
