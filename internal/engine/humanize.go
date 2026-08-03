package engine

import (
	"fmt"
	"time"
)

// humanDuration renders a duration the way an operator wants to read it in
// a log line - "2h15m", "5m30s", "45s" - rather than Go's default
// (time.Duration.String() would print sub-second precision keep-at never
// needs here, e.g. "2h15m3.219841s").
func humanDuration(d time.Duration) string {
	if d <= 0 {
		return "0s"
	}
	d = d.Round(time.Second)

	h := d / time.Hour
	d -= h * time.Hour
	m := d / time.Minute
	d -= m * time.Minute
	s := d / time.Second

	switch {
	case h > 0:
		return fmt.Sprintf("%dh%dm", h, m)
	case m > 0:
		return fmt.Sprintf("%dm%ds", m, s)
	default:
		return fmt.Sprintf("%ds", s)
	}
}
