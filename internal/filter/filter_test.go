package filter

import (
	"testing"
	"time"
)

func TestKeywordBlocklistMatchesTitleAndDescription(t *testing.T) {
	b := NewKeywordBlocklist([]string{" Confidential ", "SECRET", ""})

	cases := []struct {
		title, desc string
		wantBlocked bool
		wantMatch   string
	}{
		{"A Confidential Report", "nothing special", true, "confidential"},
		{"Public Dataset", "this one is SECRET data", true, "secret"},
		{"Totally Fine Dataset", "nothing to see here", false, ""},
	}

	for _, c := range cases {
		blocked, match := b.Blocks(c.title, c.desc)
		if blocked != c.wantBlocked || match != c.wantMatch {
			t.Errorf("Blocks(%q, %q) = (%v, %q), want (%v, %q)", c.title, c.desc, blocked, match, c.wantBlocked, c.wantMatch)
		}
	}
}

func TestAgeEligible(t *testing.T) {
	now := time.Date(2026, 1, 8, 0, 0, 0, 0, time.UTC)
	minAge := 7 * 24 * time.Hour

	cases := []struct {
		name      string
		createdAt time.Time
		minAge    time.Duration
		want      bool
	}{
		{"exactly at boundary", now.Add(-minAge), minAge, true},
		{"older than boundary", now.Add(-minAge - time.Hour), minAge, true},
		{"younger than boundary", now.Add(-minAge + time.Hour), minAge, false},
		{"unknown age", time.Time{}, minAge, false},
		{"age gate disabled, old torrent", now.Add(-minAge), 0, true},
		{"age gate disabled, brand new torrent", now, 0, true},
		{"age gate disabled, unknown age", time.Time{}, 0, true},
	}

	for _, c := range cases {
		got := AgeEligible(c.createdAt, c.minAge, now)
		if got != c.want {
			t.Errorf("%s: AgeEligible() = %v, want %v", c.name, got, c.want)
		}
	}
}
