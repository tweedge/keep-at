package buildinfo

import "testing"

func TestIsKeepAtSeeder(t *testing.T) {
	// Seeder variants (current and older versions without a role suffix)
	// must count as keep-at seeders.
	seeders := []string{
		"keep-at/0.3.5 (seeder)",
		"keep-at/0.3.4",
		"keep-at/dev",
		"keep-at/0.3.5 (seeder) (+https://github.com/tweedge/keep-at)",
	}
	for _, name := range seeders {
		if !IsKeepAtSeeder(name) {
			t.Errorf("IsKeepAtSeeder(%q) = false, want true", name)
		}
	}

	// Scraper variants must NOT count as seeders.
	notSeeders := []string{
		"keep-at/0.3.5 (scraper)",
		"keep-at/0.3.5 (scraper) (+https://github.com/tweedge/keep-at)",
		"anacrolix-torrent/v1.60.0",
		"",
		"Transmission 4.0.5",
	}
	for _, name := range notSeeders {
		if IsKeepAtSeeder(name) {
			t.Errorf("IsKeepAtSeeder(%q) = true, want false", name)
		}
	}
}

func TestRoleClientStrings(t *testing.T) {
	Version = "0.3.5"
	t.Cleanup(func() { Version = "dev" })

	if got := ExtendedHandshakeVersion(); got != "keep-at/0.3.5 (seeder)" {
		t.Errorf("ExtendedHandshakeVersion() = %q", got)
	}
	if got := ScraperExtendedHandshakeVersion(); got != "keep-at/0.3.5 (scraper)" {
		t.Errorf("ScraperExtendedHandshakeVersion() = %q", got)
	}
	if got := SeederUserAgent(); got != "keep-at/0.3.5 (seeder) (+https://github.com/tweedge/keep-at)" {
		t.Errorf("SeederUserAgent() = %q", got)
	}
	if got := ScraperUserAgent(); got != "keep-at/0.3.5 (scraper) (+https://github.com/tweedge/keep-at)" {
		t.Errorf("ScraperUserAgent() = %q", got)
	}
}
