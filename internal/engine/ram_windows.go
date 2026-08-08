package engine

import (
	"fmt"
	"unsafe"

	"golang.org/x/sys/windows"
)

// memoryStatusEx mirrors the Windows MEMORYSTATUSEX structure for a direct
// kernel32.GlobalMemoryStatusEx call. golang.org/x/sys/windows doesn't expose
// this type, so we define the minimal shape we need and call it through the
// lazy system DLL.
type memoryStatusEx struct {
	Length               uint32
	MemoryLoad           uint32
	TotalPhys            uint64
	AvailPhys            uint64
	TotalPageFile        uint64
	AvailPageFile        uint64
	TotalVirtual         uint64
	AvailVirtual         uint64
	AvailExtendedVirtual uint64
}

// SystemTotalRAM returns the host's total physical memory in bytes on Windows,
// via kernel32.GlobalMemoryStatusEx.
func SystemTotalRAM() (int64, error) {
	proc := windows.NewLazySystemDLL("kernel32.dll").NewProc("GlobalMemoryStatusEx")
	var m memoryStatusEx
	m.Length = uint32(unsafe.Sizeof(m))
	r, _, err := proc.Call(uintptr(unsafe.Pointer(&m)))
	if r == 0 {
		return 0, fmt.Errorf("engine: GlobalMemoryStatusEx failed: %w", err)
	}
	return int64(m.TotalPhys), nil
}
