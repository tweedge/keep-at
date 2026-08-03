package config

import (
	"os"
	"path/filepath"
	"testing"
)

func TestParseByteSize(t *testing.T) {
	cases := []struct {
		in   string
		want ByteSize
	}{
		{"500M", 500 * byteSizeMega},
		{"2G", 2 * byteSizeGiga},
		{"1T", byteSizeTera},
		{"1P", byteSizePeta},
		{"  10g ", 10 * byteSizeGiga},
	}
	for _, c := range cases {
		got, err := ParseByteSize(c.in)
		if err != nil {
			t.Errorf("ParseByteSize(%q): unexpected error: %v", c.in, err)
			continue
		}
		if got != c.want {
			t.Errorf("ParseByteSize(%q) = %v, want %v", c.in, got, c.want)
		}
	}
}

func TestParseByteSizeRejectsInvalid(t *testing.T) {
	invalid := []string{"", "500", "500X", "-5G", "abc"}
	for _, in := range invalid {
		if _, err := ParseByteSize(in); err == nil {
			t.Errorf("ParseByteSize(%q): expected an error, got none", in)
		}
	}
}

func TestByteSizeStringRoundTrips(t *testing.T) {
	cases := []struct {
		in   ByteSize
		want string
	}{
		{500 * byteSizeMega, "500M"},
		{2 * byteSizeGiga, "2G"},
		{byteSizeTera, "1T"},
	}
	for _, c := range cases {
		if got := c.in.String(); got != c.want {
			t.Errorf("ByteSize(%d).String() = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestValidateRejectsNoStorageLocations(t *testing.T) {
	cfg := Default()
	if err := cfg.Validate(); err == nil {
		t.Fatalf("expected Validate to reject a config with no storage locations")
	}
}

func TestValidateAcceptsWellFormedConfig(t *testing.T) {
	cfg := Default()
	cfg.Storage.Locations = []StorageLocation{{Path: "/data", Limit: byteSizeGiga}}
	if err := cfg.Validate(); err != nil {
		t.Fatalf("expected a well-formed config to validate, got: %v", err)
	}
}

func TestValidateRejectsBadAggressiveness(t *testing.T) {
	cfg := Default()
	cfg.Storage.Locations = []StorageLocation{{Path: "/data", Limit: byteSizeGiga}}
	for _, bad := range []float64{0, 1, -0.5, 1.5} {
		cfg.Aggressiveness = bad
		if err := cfg.Validate(); err == nil {
			t.Errorf("aggressiveness=%v: expected Validate to reject", bad)
		}
	}
}

func TestLoadWritesStarterConfigWhenMissing(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "mimis.yaml")

	_, err := Load(path)
	if err == nil {
		t.Fatalf("expected an error prompting the operator to fill in storage locations")
	}
	if _, statErr := os.Stat(path); statErr != nil {
		t.Fatalf("expected a starter config to be written: %v", statErr)
	}
}

func TestLoadParsesAndValidatesExistingConfig(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "mimis.yaml")
	contents := "port: 40000\n" +
		"aggressiveness: 0.6\n" +
		"storage:\n" +
		"  locations:\n" +
		"    - path: /data\n" +
		"      limit: \"100G\"\n" +
		"scan:\n" +
		"  interval: 168h\n" +
		"  rate_limit_per_second: 0.5\n" +
		"  min_seed_margin: 3\n" +
		"  moderation_delay: 168h\n"
	if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
		t.Fatalf("writing test config: %v", err)
	}

	cfg, err := Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.Port != 40000 {
		t.Errorf("Port = %d, want 40000", cfg.Port)
	}
	if len(cfg.Storage.Locations) != 1 || cfg.Storage.Locations[0].Limit != 100*byteSizeGiga {
		t.Errorf("unexpected storage locations: %+v", cfg.Storage.Locations)
	}
}
