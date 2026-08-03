package main

import (
	"flag"
	"fmt"
	"os"

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
		return cmdServiceUninstall(args[1:])
	default:
		return fmt.Errorf("usage: keep-at service <install|uninstall>")
	}
}

// cmdServiceInstall takes the same flags as `run`, resolves and validates
// them up front (so a bad flag fails now, not after the service is
// installed and systemd is silently restart-looping it), then writes the
// resolved config to service.ConfigPath and points the systemd unit at
// it. That's what lets stop/status/network-status - and a bare
// run/start - find a running keep-at without needing --config or
// --data-dir passed every time: they check service.ConfigPath
// automatically once it exists.
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
		Config:   cfg,
		User:     *user,
	}); err != nil {
		return err
	}

	fmt.Printf("keep-at service installed and started, config at %s\n", service.ConfigPath)
	return nil
}

func cmdServiceUninstall(args []string) error {
	fs := flag.NewFlagSet("service uninstall", flag.ContinueOnError)
	if err := fs.Parse(args); err != nil {
		return err
	}

	if err := service.Uninstall(); err != nil {
		return err
	}

	fmt.Printf("keep-at service uninstalled (config left in place at %s)\n", service.ConfigPath)
	return nil
}
