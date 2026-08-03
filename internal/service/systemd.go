// Package service installs and uninstalls keep-at as a systemd service on
// Linux. Other init systems (launchd, Windows' SCM) aren't implemented yet.
package service

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
	"text/template"
)

const unitPath = "/etc/systemd/system/keep-at.service"

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

[Install]
WantedBy=multi-user.target
`

var unitTemplate = template.Must(template.New("keep-at.service").Parse(unitTemplateSrc))

// InstallOpts controls how the generated systemd unit runs keep-at.
type InstallOpts struct {
	ExecPath string
	// RunArgs are the arguments to pass to ExecPath, typically starting
	// with "run" followed by whatever flags reproduce the desired config -
	// see cmd/keep-at's configToRunArgs.
	RunArgs []string
	User    string
}

type unitTemplateData struct {
	ExecStartLine string
	User          string
}

// Install writes a systemd unit for keep-at, then enables and starts it.
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

	data := unitTemplateData{
		ExecStartLine: execStartLine(opts.ExecPath, opts.RunArgs),
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
// root. It does not delete any downloaded torrent data.
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
