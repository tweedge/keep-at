package daemonctl

import (
	"os"
	"strings"
)

// IsContainerized makes a best-effort guess at whether mimis is running
// inside a container, so the CLI can default to foreground mode there -
// daemonizing (forking into the background and exiting) inside a container
// just kills the container the moment the parent process exits.
func IsContainerized() bool {
	if _, err := os.Stat("/.dockerenv"); err == nil {
		return true
	}
	data, err := os.ReadFile("/proc/1/cgroup")
	if err != nil {
		return false
	}
	content := string(data)
	return strings.Contains(content, "docker") || strings.Contains(content, "containerd") || strings.Contains(content, "kubepods")
}
