// Package config defines keep-at's configuration: where to store data, how
// much space to use, how aggressively to chase under-seeded torrents, and
// what to refuse to touch. There's a smart default storage location per
// OS, but no default space limit - keep-at never guesses how much of a
// user's disk it's allowed to eat.
//
// A config file is optional. Every field here has a command-line flag
// equivalent (see cmd/keep-at); the YAML file is only worth reaching for
// once you want more than one storage location or want settings to
// survive without repeating flags every time.
package config

import (
	"fmt"
	"os"
	"time"

	"gopkg.in/yaml.v3"
)

// DefaultPort was picked randomly during development and checked against
// the common well-known/service ports so a fresh install doesn't collide
// with something else on the box. It's just a default; override it with
// `--port` or `port` in a config file.
const DefaultPort = 37550

// DefaultAggressiveness is how eager keep-at is to swap to a more urgently
// under-seeded torrent when other keep-at nodes are already piling onto
// it. Lower values back off faster as more keep-at nodes join a swarm.
const DefaultAggressiveness = 0.6

// DefaultMinSeedMargin is how many fewer seeds a candidate torrent needs
// relative to what it would displace before keep-at considers swapping at
// all.
const DefaultMinSeedMargin = 3

// DefaultScanInterval and DefaultModerationDelay both default to one week:
// how often keep-at rescans the catalog, and how long a torrent must have
// existed before keep-at is willing to download it. Seven days gives
// Academic Torrents' moderators time to catch anything that shouldn't be
// there.
var (
	DefaultScanInterval    = Duration(7 * 24 * time.Hour)
	DefaultModerationDelay = Duration(7 * 24 * time.Hour)
)

const DefaultRateLimitPerSec = 0.5

// Config is the full keep-at configuration.
type Config struct {
	Port                    int           `yaml:"port"`
	DataDir                 string        `yaml:"data_dir"`
	Storage                 StorageConfig `yaml:"storage"`
	Scan                    ScanConfig    `yaml:"scan"`
	Aggressiveness          float64       `yaml:"aggressiveness"`
	KeywordBlocklist        []string      `yaml:"keyword_blocklist"`
	PreserveDeletedTorrents bool          `yaml:"preserve_deleted_torrents"`
}

// StorageLocation is one folder keep-at is allowed to fill, up to Limit.
type StorageLocation struct {
	Path  string   `yaml:"path"`
	Limit ByteSize `yaml:"limit"`
}

// StorageConfig lists every location keep-at may write to. It's
// intentionally empty by default; see Config's doc comment. Multiple
// locations are a config-file feature - the CLI flags only manage one.
type StorageConfig struct {
	Locations []StorageLocation `yaml:"locations"`
}

// ScanConfig controls how keep-at walks the Academic Torrents catalog and
// how carefully it rate-limits itself while doing so.
type ScanConfig struct {
	Interval           Duration `yaml:"interval"`
	RateLimitPerSecond float64  `yaml:"rate_limit_per_second"`
	MinSeedMargin      int      `yaml:"min_seed_margin"`
	ModerationDelay    Duration `yaml:"moderation_delay"`
}

// Default returns a config with every field that has a sane default filled
// in. Storage.Locations is left empty; callers must add at least one
// before the config is usable, and Validate will say so.
func Default() Config {
	return Config{
		Port:           DefaultPort,
		DataDir:        DefaultDataDir(),
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
// it, rather than silently running with no storage limit. This is the
// advanced/opt-in path: most users don't need a config file at all, see
// cmd/keep-at's flags.
func Load(path string) (Config, error) {
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		if writeErr := writeStarterConfig(path); writeErr != nil {
			return Config{}, fmt.Errorf("no config at %s, and failed to write a starter one: %w", path, writeErr)
		}
		return Config{}, fmt.Errorf("wrote a starter config to %s: set at least one storage location and limit, then run keep-at again", path)
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
// enforcing "no free default storage limit" and "aggressiveness/margins
// make sense."
func (c Config) Validate() error {
	if len(c.Storage.Locations) == 0 {
		return fmt.Errorf("no storage locations configured; keep-at will not pick a default limit, set --storage-limit (and optionally --storage) or storage.locations in a config file")
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
		{Path: DefaultStorageLocation(), Limit: 0},
	}
	out, err := yaml.Marshal(starter)
	if err != nil {
		return err
	}
	header := "# keep-at starter config. This is entirely optional - every field here has\n" +
		"# a --flag equivalent (run `keep-at run --help`). Reach for a config file\n" +
		"# once you want more than one storage location, or don't want to repeat\n" +
		"# flags every time.\n" +
		"#\n" +
		"# At minimum, set a real limit (e.g. 500G, 2T) below.\n\n"
	return os.WriteFile(path, append([]byte(header), out...), 0o644)
}

// DefaultDataDir is where keep-at keeps its own bookkeeping (state,
// logs, cached torrent metadata) - not the torrent data itself, see
// DefaultStorageLocation.
func DefaultDataDir() string {
	return platformAppDir("keep-at")
}

// DefaultStorageLocation is the OS-appropriate default place keep-at
// stores torrent data if --storage isn't given. There's still no default
// space *limit* - see Validate.
func DefaultStorageLocation() string {
	return platformAppDir("keep-at") + string(os.PathSeparator) + "storage"
}
