package main

import (
	"flag"
	"fmt"
	"strings"
	"time"

	"github.com/tweedge/keep-at/internal/config"
)

// configFlagSet mirrors every config.Config field as a command-line flag,
// so a config file is optional: `keep-at run --storage-limit 500G` is
// enough to get going, and the file (see --config) is only worth reaching
// for once you want more than one storage location or don't want to
// repeat flags every time.
type configFlagSet struct {
	configPath              *string
	storage                 *string
	storageLimit            *string
	port                    *int
	dataDir                 *string
	aggressiveness          *float64
	minSeedMargin           *int
	scanInterval            *time.Duration
	moderationDelay         *time.Duration
	rateLimit               *float64
	keywordBlocklist        *string
	preserveDeletedTorrents *bool
}

func addConfigFlags(fs *flag.FlagSet) *configFlagSet {
	def := config.Default()
	return &configFlagSet{
		configPath:              fs.String("config", "", "path to a config file (optional; advanced/multi-location setups only)"),
		storage:                 fs.String("storage", config.DefaultStorageLocation(), "storage location for torrent data"),
		storageLimit:            fs.String("storage-limit", "", "how much space to use, e.g. 500G, 2T (required unless set in --config)"),
		port:                    fs.Int("port", def.Port, "BitTorrent listen port"),
		dataDir:                 fs.String("data-dir", def.DataDir, "directory for keep-at's own state, logs, and cached metadata"),
		aggressiveness:          fs.Float64("aggressiveness", def.Aggressiveness, "anti-cascade base (0-1); lower backs off faster as more keep-at nodes join a swarm"),
		minSeedMargin:           fs.Int("min-seed-margin", def.Scan.MinSeedMargin, "how many fewer seeds a candidate needs before displacing a held torrent"),
		scanInterval:            fs.Duration("scan-interval", def.Scan.Interval.AsDuration(), "how often to rescan the Academic Torrents catalog"),
		moderationDelay:         fs.Duration("moderation-delay", def.Scan.ModerationDelay.AsDuration(), "minimum torrent age before keep-at will download it"),
		rateLimit:               fs.Float64("rate-limit", def.Scan.RateLimitPerSecond, "max requests per second to Academic Torrents' own infrastructure"),
		keywordBlocklist:        fs.String("keyword-blocklist", "", "comma-separated keywords to block, matched against title and description"),
		preserveDeletedTorrents: fs.Bool("preserve-deleted-torrents", def.PreserveDeletedTorrents, "keep seeding a torrent even if Academic Torrents removes it"),
	}
}

// resolve builds a validated Config from whatever combination of --config
// and other flags the user actually passed. fs is used to tell "flag left
// at its default" apart from "flag explicitly set to the same value as the
// default," so a loaded config file's fields aren't clobbered by flags the
// user didn't touch.
func (cf *configFlagSet) resolve(fs *flag.FlagSet) (config.Config, error) {
	cfg := config.Default()
	configFileLoaded := false

	if *cf.configPath != "" {
		loaded, err := config.Load(*cf.configPath)
		if err != nil {
			return config.Config{}, err
		}
		cfg = loaded
		configFileLoaded = true
	}

	storageFlagsSet := false
	fs.Visit(func(fl *flag.Flag) {
		switch fl.Name {
		case "port":
			cfg.Port = *cf.port
		case "data-dir":
			cfg.DataDir = *cf.dataDir
		case "aggressiveness":
			cfg.Aggressiveness = *cf.aggressiveness
		case "min-seed-margin":
			cfg.Scan.MinSeedMargin = *cf.minSeedMargin
		case "scan-interval":
			cfg.Scan.Interval = config.Duration(*cf.scanInterval)
		case "moderation-delay":
			cfg.Scan.ModerationDelay = config.Duration(*cf.moderationDelay)
		case "rate-limit":
			cfg.Scan.RateLimitPerSecond = *cf.rateLimit
		case "keyword-blocklist":
			cfg.KeywordBlocklist = splitKeywords(*cf.keywordBlocklist)
		case "preserve-deleted-torrents":
			cfg.PreserveDeletedTorrents = *cf.preserveDeletedTorrents
		case "storage", "storage-limit":
			storageFlagsSet = true
		}
	})

	if storageFlagsSet {
		if configFileLoaded {
			return config.Config{}, fmt.Errorf("--storage/--storage-limit can't be combined with --config; multiple storage locations are a config-file-only feature, edit %s instead", *cf.configPath)
		}
		if *cf.storageLimit == "" {
			return config.Config{}, fmt.Errorf("--storage-limit is required (e.g. --storage-limit 500G)")
		}
		limit, err := config.ParseByteSize(*cf.storageLimit)
		if err != nil {
			return config.Config{}, err
		}
		cfg.Storage.Locations = []config.StorageLocation{{Path: *cf.storage, Limit: limit}}
	} else if !configFileLoaded && len(cfg.Storage.Locations) == 0 {
		return config.Config{}, fmt.Errorf("no storage configured; pass --storage-limit (e.g. --storage-limit 500G) or --config")
	}

	if err := cfg.Validate(); err != nil {
		return config.Config{}, err
	}
	return cfg, nil
}

func splitKeywords(raw string) []string {
	if strings.TrimSpace(raw) == "" {
		return nil
	}
	parts := strings.Split(raw, ",")
	out := make([]string, 0, len(parts))
	for _, p := range parts {
		if trimmed := strings.TrimSpace(p); trimmed != "" {
			out = append(out, trimmed)
		}
	}
	return out
}
