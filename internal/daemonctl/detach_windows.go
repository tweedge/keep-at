//go:build windows

package daemonctl

import (
	"os/exec"
	"syscall"
)

// detachFromParent starts the child in a new process group so closing this
// process' console doesn't also signal the child. Windows service
// management proper isn't implemented (see internal/service); this just
// keeps `keep-at start` functional for basic background use on Windows.
func detachFromParent(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{CreationFlags: syscall.CREATE_NEW_PROCESS_GROUP}
}
