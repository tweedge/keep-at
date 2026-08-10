// Package service installs and uninstalls keep-at as a systemd service on
// Linux. Other init systems (launchd, Windows' SCM) aren't implemented yet.
package service

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
	"text/template"

	"github.com/tweedge/keep-at/internal/config"
)

const unitPath = "/etc/systemd/system/keep-at.service"

// ConfigDir and ConfigPath are where `service install` writes the
// resolved config, alongside the systemd unit itself. Once installed,
// this is the one place every other command (stop/status/network-status,
// and a bare `run`/`start` with no flags) looks for keep-at's settings, so
// none of them need --config or --data-dir passed by hand.
const (
	ConfigDir  = "/etc/keep-at"
	ConfigPath = ConfigDir + "/config.yaml"
)

const unitTemplateSrc = `[Unit]
Description=keep-at - Academic Torrents smart seeding node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={{.ExecStartLine}}
Restart=on-failure
RestartSec=5
User={{.User}}
# keep-at handles SIGTERM itself and stops promptly even mid-scan (see
# ScanOnce's ctx-aware drain loop), but this caps how long systemd will wait
# before force-killing it if something ever hangs, so 'systemctl stop
# keep-at' can't block forever on a stuck process.
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
`

var unitTemplate = template.Must(template.New("keep-at.service").Parse(unitTemplateSrc))

// InstallOpts controls how the generated systemd unit runs keep-at.
type InstallOpts struct {
	ExecPath string
	// Config is written to ConfigPath and is what the installed unit
	// runs with - see cmd/keep-at's `service install`, which resolves
	// this from whatever combination of flags and/or an existing config
	// file the operator passed.
	Config config.Config
	User   string
}

type unitTemplateData struct {
	ExecStartLine string
	User          string
}

// Install writes the resolved config to ConfigPath, writes a systemd unit
// that runs keep-at against it, then enables and starts the service.
// Requires root.
func Install(opts InstallOpts) error {
	if err := requireRoot(); err != nil {
		return err
	}
	if err := requireSystemd(); err != nil {
		return err
	}

	if err := config.Save(ConfigPath, opts.Config); err != nil {
		return fmt.Errorf("service: writing %s: %w", ConfigPath, err)
	}

	f, err := os.Create(unitPath)
	if err != nil {
		return fmt.Errorf("service: creating %s: %w", unitPath, err)
	}
	defer f.Close()

	data := unitTemplateData{
		ExecStartLine: execStartLine(opts.ExecPath, []string{"run", "--config", ConfigPath}),
		User:          opts.User,
	}
	if err := unitTemplate.Execute(f, data); err != nil {
		return fmt.Errorf("service: rendering unit file: %w", err)
	}

	for _, args := range [][]string{
		{"daemon-reload"},
		{"enable", "keep-at"},
		{"start", "keep-at"},
	} {
		if err := runSystemctl(args...); err != nil {
			return err
		}
	}

	return nil
}

// Uninstall stops, disables, and removes keep-at's systemd unit. Requires
// root. It does not delete any downloaded torrent data, and leaves
// ConfigPath in place in case the operator wants to reinstall or refer
// back to it.
func Uninstall() error {
	if err := requireRoot(); err != nil {
		return err
	}
	if err := requireSystemd(); err != nil {
		return err
	}

	_ = runSystemctl("stop", "keep-at")
	_ = runSystemctl("disable", "keep-at")

	if err := os.Remove(unitPath); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("service: removing %s: %w", unitPath, err)
	}

	return runSystemctl("daemon-reload")
}

// execStartLine joins execPath and args into a single systemd ExecStart=
// value, double-quoting any argument that contains whitespace (systemd
// splits ExecStart on unquoted whitespace, same as a shell would).
func execStartLine(execPath string, args []string) string {
	parts := make([]string, 0, len(args)+1)
	parts = append(parts, quoteIfNeeded(execPath))
	for _, a := range args {
		parts = append(parts, quoteIfNeeded(a))
	}
	return strings.Join(parts, " ")
}

func quoteIfNeeded(s string) string {
	if !strings.ContainsAny(s, " \t\"") {
		return s
	}
	return `"` + strings.ReplaceAll(s, `"`, `\"`) + `"`
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
