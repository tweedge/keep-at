// Package config defines mimis' on-disk configuration: where to store data,
// how much space to use, how aggressively to chase under-seeded torrents,
// and what to refuse to touch. Nothing in here has a default storage
// location or space limit, on purpose - the plan is explicit that mimis
// must never guess how much of a user's disk it's allowed to eat.
package config

import (
	"fmt"
	"os"
	"time"

	"gopkg.in/yaml.v3"
)

// DefaultPort was picked randomly during development (see PLAN.md) and
// checked against the common well-known/service ports so a fresh install
// doesn't collide with something else on the box. It's just a default;
// override it with `port` in the config file.
const DefaultPort = 37550

// DefaultAggressiveness is how eager mimis is to swap to a more urgently
// under-seeded torrent when other mimis nodes are already piling onto it.
// Lower values back off faster as more mimis nodes join a swarm.
const DefaultAggressiveness = 0.6

// DefaultMinSeedMargin is how many fewer seeds a candidate torrent needs
// relative to what it would displace before mimis considers swapping at all.
const DefaultMinSeedMargin = 3

// DefaultScanInterval and DefaultModerationDelay both default to one week:
// how often mimis rescans the catalog, and how long a torrent must have
// existed before mimis is willing to download it. See PLAN.md for why 7
// days: it gives Academic Torrents' moderators time to catch anything that
// shouldn't be there.
var (
	DefaultScanInterval    = Duration(7 * 24 * time.Hour)
	DefaultModerationDelay = Duration(7 * 24 * time.Hour)
)

const DefaultRateLimitPerSec = 0.5

// Config is the full mimis configuration, loaded from a YAML file.
type Config struct {
	Port                    int           `yaml:"port"`
	DataDir                 string        `yaml:"data_dir"`
	Storage                 StorageConfig `yaml:"storage"`
	Scan                    ScanConfig    `yaml:"scan"`
	Aggressiveness          float64       `yaml:"aggressiveness"`
	KeywordBlocklist        []string      `yaml:"keyword_blocklist"`
	PreserveDeletedTorrents bool          `yaml:"preserve_deleted_torrents"`
}

// StorageLocation is one folder mimis is allowed to fill, up to Limit.
type StorageLocation struct {
	Path  string   `yaml:"path"`
	Limit ByteSize `yaml:"limit"`
}

// StorageConfig lists every location mimis may write to. It's intentionally
// empty by default; see Config's doc comment.
type StorageConfig struct {
	Locations []StorageLocation `yaml:"locations"`
}

// ScanConfig controls how mimis walks the Academic Torrents catalog and how
// carefully it rate-limits itself while doing so.
type ScanConfig struct {
	Interval           Duration `yaml:"interval"`
	RateLimitPerSecond float64  `yaml:"rate_limit_per_second"`
	MinSeedMargin      int      `yaml:"min_seed_margin"`
	ModerationDelay    Duration `yaml:"moderation_delay"`
}

// Default returns a config with every field that has a sane default filled
// in. Storage.Locations is left empty; callers must add at least one before
// the config is usable, and Validate will say so.
func Default() Config {
	return Config{
		Port:           DefaultPort,
		DataDir:        defaultDataDir(),
		Aggressiveness: DefaultAggressiveness,
		Scan: ScanConfig{
			Interval:           DefaultScanInterval,
			RateLimitPerSecond: DefaultRateLimitPerSec,
			MinSeedMargin:      DefaultMinSeedMargin,
			ModerationDelay:    DefaultModerationDelay,
		},
	}
}

// Load reads and validates a config file at path. If the file doesn't
// exist, it writes out a starter config (storage locations left blank for
// the operator to fill in) and returns an error telling the caller to edit
// it, rather than silently running with no storage limit.
func Load(path string) (Config, error) {
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		if writeErr := writeStarterConfig(path); writeErr != nil {
			return Config{}, fmt.Errorf("no config at %s, and failed to write a starter one: %w", path, writeErr)
		}
		return Config{}, fmt.Errorf("wrote a starter config to %s: set at least one storage location and limit, then run mimis again", path)
	}
	if err != nil {
		return Config{}, fmt.Errorf("reading config %s: %w", path, err)
	}

	cfg := Default()
	if err := yaml.Unmarshal(data, &cfg); err != nil {
		return Config{}, fmt.Errorf("parsing config %s: %w", path, err)
	}

	if err := cfg.Validate(); err != nil {
		return Config{}, fmt.Errorf("config %s: %w", path, err)
	}

	return cfg, nil
}

// Validate checks that the config is safe to run with. It's the one place
// enforcing "no free default storage" and "aggressiveness/margins make
// sense."
func (c Config) Validate() error {
	if len(c.Storage.Locations) == 0 {
		return fmt.Errorf("no storage locations configured; mimis will not pick a default, add at least one under storage.locations")
	}
	seen := make(map[string]bool, len(c.Storage.Locations))
	for _, loc := range c.Storage.Locations {
		if loc.Path == "" {
			return fmt.Errorf("storage location has an empty path")
		}
		if loc.Limit <= 0 {
			return fmt.Errorf("storage location %s has no positive limit set", loc.Path)
		}
		if seen[loc.Path] {
			return fmt.Errorf("storage location %s is listed more than once", loc.Path)
		}
		seen[loc.Path] = true
	}
	if c.Aggressiveness <= 0 || c.Aggressiveness >= 1 {
		return fmt.Errorf("aggressiveness must be between 0 and 1 (exclusive), got %v", c.Aggressiveness)
	}
	if c.Scan.MinSeedMargin < 0 {
		return fmt.Errorf("scan.min_seed_margin must not be negative")
	}
	if c.Port <= 0 || c.Port > 65535 {
		return fmt.Errorf("port %d is out of range", c.Port)
	}
	if c.Scan.RateLimitPerSecond <= 0 {
		return fmt.Errorf("scan.rate_limit_per_second must be positive")
	}
	return nil
}

func writeStarterConfig(path string) error {
	starter := Default()
	starter.Storage.Locations = []StorageLocation{
		{Path: "/path/to/storage", Limit: 0},
	}
	out, err := yaml.Marshal(starter)
	if err != nil {
		return err
	}
	header := "# mimis starter config. At minimum, replace storage.locations with a real\n" +
		"# path and a limit (e.g. 500G, 2T). mimis will not run without at least one.\n\n"
	return os.WriteFile(path, append([]byte(header), out...), 0o644)
}

func defaultDataDir() string {
	if dir, err := os.UserHomeDir(); err == nil && dir != "" {
		return dir + "/.local/share/mimisbaeti"
	}
	return "/var/lib/mimisbaeti"
}
