package netstats

import "fmt"

var byteUnits = []struct {
	suffix string
	size   float64
}{
	{"PB", 1 << 50},
	{"TB", 1 << 40},
	{"GB", 1 << 30},
	{"MB", 1 << 20},
	{"KB", 1 << 10},
}

var bpsUnits = []struct {
	suffix string
	size   float64
}{
	{"gbps", 1e9},
	{"mbps", 1e6},
	{"kbps", 1e3},
}

// HumanBytes renders a byte count the way an operator wants to read it on
// a terminal: one decimal place, largest sensible unit, e.g. "1.5 GB"
// rather than a raw byte count or config's exact-round-trip "2G" form.
func HumanBytes(n int64) string {
	if n < 0 {
		return "0 B"
	}
	f := float64(n)
	for _, u := range byteUnits {
		if f >= u.size {
			return fmt.Sprintf("%.1f %s", f/u.size, u.suffix)
		}
	}
	return fmt.Sprintf("%d B", n)
}

// HumanBitsPerSec renders a rate in bits per second the way an operator
// wants to read it, e.g. "1.5 mbps" or "512 kbps". Sub-kilobit rates are
// shown in plain bps. A negative rate is treated as 0.
func HumanBitsPerSec(bps float64) string {
	if bps <= 0 {
		return "0 bps"
	}
	for _, u := range bpsUnits {
		if bps >= u.size {
			return fmt.Sprintf("%.1f %s", bps/u.size, u.suffix)
		}
	}
	return fmt.Sprintf("%.0f bps", bps)
}
