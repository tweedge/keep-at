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
	"path/filepath"
	"strings"
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

// DefaultRateLimitPerSec is the default maximum request rate to Academic
// Torrents' own infrastructure (tracker announces, .torrent fetches, and the
// catalog download).
const DefaultRateLimitPerSec = 0.5

// DefaultStatsInterval is how often keep-at logs and persists a summary of
// what it's doing, so operators (and `keep-at status`) can see held torrents,
// disk utilization, and transfer totals without digging through logs. 30
// minutes is frequent enough to be useful but not so chatty it buries real
// messages.
const DefaultStatsInterval = Duration(30 * time.Minute)

// DefaultStallEvictionTimeout is how long a held torrent can sit with zero
// seeders and no download progress before keep-at gives up on it and removes
// it to free the slot for a torrent that can actually complete. Defaults to
// two weeks - deliberately cautious, since a torrent with no seeders today
// might gain some tomorrow, and keep-at's whole job is to seed exactly the
// under-seeded torrents everyone else ignores.
var DefaultStallEvictionTimeout = Duration(14 * 24 * time.Hour)

// AllLimitFraction is what fraction of a storage device's total (formatted)
// capacity `limit: all` resolves to. Analysis:
//
//   - The device total (statfs Blocks * Bsize) is the post-formatting
//     capacity, not the raw disk size.
//   - Filesystems reserve blocks for their own health: ext4 defaults to
//     reserving 5% for root, and journals/metadata need room too. A daemon
//     running as root can fill past what a normal user could, so the
//     fraction has to be well under 100%.
//   - keep-at's space accounting counts gzip-compressed on-disk bytes, but
//     filesystems allocate in blocks, so many small piece files cost more
//     real space than their byte sum (block slack). That argues for a little
//     extra headroom beyond the reserved blocks.
//
// 97.5% is the right balance for a dedicated drive: it uses nearly all of the
// device while leaving 2.5% (plus whatever the filesystem reserves) for the
// journal, metadata, block slack, and the OS's own emergency operations.
// It's documented as dedicated-drive-only precisely because the margin is
// thin; on a busy OS drive that same 2.5% isn't enough headroom for the OS.
const AllLimitFraction = 0.975

// Config is the full keep-at configuration.
type Config struct {
	Port                    int           `yaml:"port"`
	DataDir                 string        `yaml:"data_dir"`
	Storage                 StorageConfig `yaml:"storage"`
	Scan                    ScanConfig    `yaml:"scan"`
	Aggressiveness          float64       `yaml:"aggressiveness"`
	KeywordBlocklist        []string      `yaml:"keyword_blocklist"`
	PreserveDeletedTorrents bool          `yaml:"preserve_deleted_torrents"`

	// MaxRAM caps how much of the host's memory keep-at will plan its
	// torrent holding around. It's expressed as a ByteSize for a config
	// file; the CLI exposes it as --max-ram. Zero (the default) means "use
	// the full hard cap of 80% of system RAM" - keep-at is designed to run
	// unattended and simply, so by default it spends up to that share and
	// figures out the rest itself. A explicit value is subject to the same
	// hard cap and is never allowed to exceed 80% of system RAM.
	MaxRAM ByteSize `yaml:"max_ram"`

	// MaxRAMConfig mirrors MaxRAM but is only set from a config file (it
	// carries the on-disk value), so the CLI's --max-ram and a config file
	// can be distinguished by resolve. It is not unmarshalled from YAML
	// directly; see Load.
	MaxRAMConfig ByteSize `yaml:"-"`

	// APIKey is the operator's Academic Torrents API key (from
	// https://academictorrents.com/my.php, formatted like
	// "uid=12345;pass=abcdef..."). When set, keep-at resolves it to the
	// per-user tracker announce URL at startup and uses that URL for every
	// announce to Academic Torrents' own trackers, which makes AT show the
	// operator's account (name and image) as hosting the torrents keep-at
	// seeds. The key is only ever sent to the two Academic Torrents tracker
	// hosts; it is never logged, never written into cached .torrent files or
	// state, and never passed to any third-party tracker.
	APIKey string `yaml:"api_key"`

	// UploadRateLimit and DownloadRateLimit cap how fast keep-at transfers
	// data, in bytes per second. Zero (the default) means unlimited. A
	// limit is applied across all torrents at once (a single shared rate
	// limiter on the torrent client), not per torrent.
	UploadRateLimit   ByteSize `yaml:"upload_rate_limit"`
	DownloadRateLimit ByteSize `yaml:"download_rate_limit"`

	// StatsInterval controls how often keep-at logs a brief summary of what
	// it's doing - torrents held/seeding, disk utilization, bytes
	// transferred since boot, and so on - and writes that summary to disk
	// so `keep-at status` can display it. Zero disables the periodic log
	// (one summary is still written at startup).
	StatsInterval Duration `yaml:"stats_interval"`
}

// StorageLocation is one folder keep-at is allowed to fill, up to Limit.
type StorageLocation struct {
	Path  string   `yaml:"path"`
	Limit ByteSize `yaml:"limit"`

	// LimitAll means "use as much of the device as possible": Limit is
	// resolved at startup to AllLimitFraction of the storage device's total
	// (formatted) capacity, measured with statfs on the location path. Set
	// from `limit: all` in a config file or `--storage-limit all` on the CLI.
	// Intended for dedicated data drives only - keep-at will fill the device
	// to the resolved fraction, which is no place for an OS.
	LimitAll bool `yaml:"-"`
}

// UnmarshalYAML lets `limit:` accept either a byte size like "500G" or the
// literal "all". "all" leaves Limit at zero and sets LimitAll, which the
// engine resolves to a concrete byte count at startup.
func (l *StorageLocation) UnmarshalYAML(unmarshal func(interface{}) error) error {
	var raw struct {
		Path  string `yaml:"path"`
		Limit string `yaml:"limit"`
	}
	if err := unmarshal(&raw); err != nil {
		return err
	}
	l.Path = raw.Path
	if strings.EqualFold(strings.TrimSpace(raw.Limit), "all") {
		l.LimitAll = true
		l.Limit = 0
		return nil
	}
	parsed, err := ParseByteSize(raw.Limit)
	if err != nil {
		return err
	}
	l.Limit = parsed
	return nil
}

// MarshalYAML writes `limit: all` back out for a location configured that
// way, so a config file keep-at writes (e.g. service install) round-trips.
func (l StorageLocation) MarshalYAML() (interface{}, error) {
	out := struct {
		Path  string `yaml:"path"`
		Limit string `yaml:"limit"`
	}{Path: l.Path}
	if l.LimitAll {
		out.Limit = "all"
	} else {
		out.Limit = l.Limit.String()
	}
	return out, nil
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

	// StallEvictionTimeout is how long a held torrent can sit with zero
	// seeders and no download progress before keep-at removes it to free the
	// slot for a torrent that can actually complete. Zero disables stalled
	// torrent eviction entirely. See DefaultStallEvictionTimeout for the
	// default rationale.
	StallEvictionTimeout Duration `yaml:"stall_eviction_timeout"`
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
			Interval:             DefaultScanInterval,
			RateLimitPerSecond:   DefaultRateLimitPerSec,
			MinSeedMargin:        DefaultMinSeedMargin,
			ModerationDelay:      DefaultModerationDelay,
			StallEvictionTimeout: DefaultStallEvictionTimeout,
		},
		StatsInterval: DefaultStatsInterval,
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
		if !loc.LimitAll && loc.Limit <= 0 {
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
	if c.UploadRateLimit < 0 {
		return fmt.Errorf("upload_rate_limit must not be negative")
	}
	if c.DownloadRateLimit < 0 {
		return fmt.Errorf("download_rate_limit must not be negative")
	}
	if c.StatsInterval < 0 {
		return fmt.Errorf("stats_interval must not be negative")
	}
	if c.Scan.StallEvictionTimeout < 0 {
		return fmt.Errorf("scan.stall_eviction_timeout must not be negative")
	}
	return nil
}

func writeStarterConfig(path string) error {
	starter := Default()
	starter.Storage.Locations = []StorageLocation{
		{Path: DefaultStorageLocation(), Limit: 0},
	}
	header := "# keep-at starter config. This is entirely optional - every field here has\n" +
		"# a --flag equivalent (run `keep-at run --help`). Reach for a config file\n" +
		"# once you want more than one storage location, or don't want to repeat\n" +
		"# flags every time.\n" +
		"#\n" +
		"# At minimum, set a real limit (e.g. 500G, 2T) below.\n" +
		"#\n" +
		"# To get credit for the torrents you seed, set api_key to your Academic\n" +
		"# Torrents API key (https://academictorrents.com/my.php) - it's only sent\n" +
		"# to AT's own trackers and never logged.\n\n"
	return writeYAML(path, header, starter)
}

// Save writes a fully-resolved config to path as YAML. Used by `keep-at
// service install` to persist whatever combination of flags and/or config
// file the operator used, alongside the systemd unit it installs, so
// other commands (and the unit itself) have exactly one place to look.
func Save(path string, cfg Config) error {
	header := "# keep-at config, installed alongside the systemd service by\n" +
		"# `keep-at service install`. Edit this file directly and run\n" +
		"# `systemctl restart keep-at` (or `keep-at service install` again) to\n" +
		"# apply changes.\n\n"
	return writeYAML(path, header, cfg)
}

func writeYAML(path, header string, cfg Config) error {
	out, err := yaml.Marshal(cfg)
	if err != nil {
		return fmt.Errorf("config: marshalling: %w", err)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return fmt.Errorf("config: creating directory for %s: %w", path, err)
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, append([]byte(header), out...), 0o644); err != nil {
		return fmt.Errorf("config: writing %s: %w", tmp, err)
	}
	if err := os.Rename(tmp, path); err != nil {
		return fmt.Errorf("config: finalizing %s: %w", path, err)
	}
	return nil
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
