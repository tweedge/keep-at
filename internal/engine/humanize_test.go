package engine

import (
	"testing"
	"time"
)

func TestHumanDuration(t *testing.T) {
	cases := []struct {
		d    time.Duration
		want string
	}{
		{0, "0s"},
		{-5 * time.Second, "0s"},
		{45 * time.Second, "45s"},
		{5*time.Minute + 30*time.Second, "5m30s"},
		{2*time.Hour + 15*time.Minute + 3*time.Second, "2h15m"},
		{59 * time.Second, "59s"},
		{90 * time.Second, "1m30s"},
	}
	for _, c := range cases {
		if got := humanDuration(c.d); got != c.want {
			t.Errorf("humanDuration(%v) = %q, want %q", c.d, got, c.want)
		}
	}
}
