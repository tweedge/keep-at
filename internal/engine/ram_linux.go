package engine

import (
	"fmt"

	"golang.org/x/sys/unix"
)

// SystemTotalRAM returns the host's total physical memory in bytes on Linux,
// using the portable unix.Sysinfo interface.
func SystemTotalRAM() (int64, error) {
	var info unix.Sysinfo_t
	if err := unix.Sysinfo(&info); err != nil {
		return 0, fmt.Errorf("engine: reading system RAM: %w", err)
	}
	// Unit is MemUnit bytes (usually 1 on modern kernels), not 1024.
	return int64(info.Totalram) * int64(info.Unit), nil
}
