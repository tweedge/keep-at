package main

import (
	"flag"
	"fmt"
	"os"

	"github.com/tweedge/mimisbaeti/internal/service"
)

func cmdService(args []string) error {
	if len(args) < 1 {
		return fmt.Errorf("usage: mimis service <install|uninstall> [--config PATH]")
	}

	switch args[0] {
	case "install":
		return cmdServiceInstall(args[1:])
	case "uninstall":
		return service.Uninstall()
	default:
		return fmt.Errorf("usage: mimis service <install|uninstall> [--config PATH]")
	}
}

func cmdServiceInstall(args []string) error {
	fs := flag.NewFlagSet("service install", flag.ContinueOnError)
	configPath := fs.String("config", defaultConfigPath(), "path to mimis config file")
	user := fs.String("user", "root", "user the systemd service runs as")
	if err := fs.Parse(args); err != nil {
		return err
	}

	execPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locating mimis executable: %w", err)
	}

	if err := service.Install(service.InstallOpts{
		ExecPath:   execPath,
		ConfigPath: *configPath,
		User:       *user,
	}); err != nil {
		return err
	}

	fmt.Println("mimis service installed and started")
	return nil
}
