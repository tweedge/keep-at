// Package service installs and uninstalls mimis as a systemd service on
// Linux. Other init systems (launchd, Windows' SCM) aren't implemented yet;
// see PLAN.md for why Linux is first-class in this version.
package service

import (
	"fmt"
	"os"
	"os/exec"
	"text/template"
)

const unitPath = "/etc/systemd/system/mimisbaeti.service"

const unitTemplateSrc = `[Unit]
Description=mimis - Academic Torrents smart seeding node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={{.ExecPath}} run --config {{.ConfigPath}}
Restart=on-failure
RestartSec=5
User={{.User}}

[Install]
WantedBy=multi-user.target
`

var unitTemplate = template.Must(template.New("mimisbaeti.service").Parse(unitTemplateSrc))

// InstallOpts controls how the generated systemd unit runs mimis.
type InstallOpts struct {
	ExecPath   string
	ConfigPath string
	User       string
}

// Install writes a systemd unit for mimis, then enables and starts it.
// Requires root.
func Install(opts InstallOpts) error {
	if err := requireRoot(); err != nil {
		return err
	}
	if err := requireSystemd(); err != nil {
		return err
	}

	f, err := os.Create(unitPath)
	if err != nil {
		return fmt.Errorf("service: creating %s: %w", unitPath, err)
	}
	defer f.Close()

	if err := unitTemplate.Execute(f, opts); err != nil {
		return fmt.Errorf("service: rendering unit file: %w", err)
	}

	for _, args := range [][]string{
		{"daemon-reload"},
		{"enable", "mimisbaeti"},
		{"start", "mimisbaeti"},
	} {
		if err := runSystemctl(args...); err != nil {
			return err
		}
	}

	return nil
}

// Uninstall stops, disables, and removes mimis' systemd unit. Requires
// root. It does not delete any downloaded torrent data.
func Uninstall() error {
	if err := requireRoot(); err != nil {
		return err
	}
	if err := requireSystemd(); err != nil {
		return err
	}

	_ = runSystemctl("stop", "mimisbaeti")
	_ = runSystemctl("disable", "mimisbaeti")

	if err := os.Remove(unitPath); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("service: removing %s: %w", unitPath, err)
	}

	return runSystemctl("daemon-reload")
}

func runSystemctl(args ...string) error {
	cmd := exec.Command("systemctl", args...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("service: systemctl %v: %w: %s", args, err, out)
	}
	return nil
}

func requireRoot() error {
	if os.Geteuid() != 0 {
		return fmt.Errorf("service: must be run as root (try sudo)")
	}
	return nil
}

func requireSystemd() error {
	if _, err := exec.LookPath("systemctl"); err != nil {
		return fmt.Errorf("service: systemctl not found; systemd service management isn't available on this system")
	}
	return nil
}
