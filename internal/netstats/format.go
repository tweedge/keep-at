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
