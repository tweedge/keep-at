package engine

import (
	"fmt"

	"golang.org/x/sys/unix"
)

// SystemTotalRAM returns the host's total physical memory in bytes on macOS,
// via the hw.memsize sysctl (TotalPhys is not exposed by unix.Sysinfo here).
func SystemTotalRAM() (int64, error) {
	n, err := unix.SysctlUint64("hw.memsize")
	if err != nil {
		return 0, fmt.Errorf("engine: reading system RAM: %w", err)
	}
	return int64(n), nil
}
