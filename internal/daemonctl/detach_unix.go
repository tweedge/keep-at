//go:build !windows

package daemonctl

import (
	"os/exec"
	"syscall"
)

// detachFromParent starts the child in its own session, so it survives
// this process exiting instead of being tied to its process group.
func detachFromParent(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
}
