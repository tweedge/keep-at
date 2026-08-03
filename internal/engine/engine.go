// Package engine ties every other package together into keep-at's actual
// runtime behavior: periodically scanning the Academic Torrents catalog,
// deciding what to seed, and running the underlying BitTorrent client.
package engine

import (
	"fmt"
	"log/slog"
	"net/http"
	"time"

	"github.com/anacrolix/torrent"
	"golang.org/x/time/rate"

	"github.com/tweedge/keep-at/internal/atcatalog"
	"github.com/tweedge/keep-at/internal/attorrent"
	"github.com/tweedge/keep-at/internal/buildinfo"
	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/filter"
	"github.com/tweedge/keep-at/internal/piecestore"
	"github.com/tweedge/keep-at/internal/state"
)

// Engine owns the BitTorrent client, per-location storage backends, and
// keep-at's persisted view of what it holds. One Engine corresponds to one
// running keep-at process.
type Engine struct {
	cfg    config.Config
	logger *slog.Logger

	torrentClient *torrent.Client
	stores        map[string]*piecestore.Client // keyed by storage location path
	probeStore    *piecestore.Client            // scratch storage for swarm-probing candidates, see probe.go
	state         *state.State

	catalogFetcher *atcatalog.Fetcher
	torrentFetcher *attorrent.Fetcher
	httpClient     *http.Client
	blocklist      filter.KeywordBlocklist
	probeTimeout   time.Duration
}

// Options carries constructor inputs that aren't part of the user-facing
// YAML config - mainly test seams like overriding the catalog or Academic
// Torrents base URL, or shortening the swarm-probe timeout so tests don't
// have to wait out the production default.
//
// CatalogURL and AcademicTorrentsBaseURL are independent: a mirror could
// serve its own copy of the catalog while .torrent files still come from
// the canonical site, or vice versa. Tests use this to serve a small,
// hand-picked catalog locally while still fetching real .torrent files and
// tracker data from the real Academic Torrents infrastructure.
type Options struct {
	CatalogURL              string
	AcademicTorrentsBaseURL string
	Logger                  *slog.Logger
	ProbeTimeout            time.Duration
}

// New builds an Engine from cfg. It opens (creating if necessary) a piece
// store per configured storage location, the keep-at state file, and the
// underlying anacrolix/torrent client. It does not start scanning; call Run
// for that.
func New(cfg config.Config, opts Options) (*Engine, error) {
	logger := opts.Logger
	if logger == nil {
		logger = slog.Default()
	}

	if err := cfg.Validate(); err != nil {
		return nil, fmt.Errorf("engine: invalid config: %w", err)
	}

	stores := make(map[string]*piecestore.Client, len(cfg.Storage.Locations))
	for _, loc := range cfg.Storage.Locations {
		store, err := piecestore.New(loc.Path)
		if err != nil {
			return nil, fmt.Errorf("engine: opening storage location %s: %w", loc.Path, err)
		}
		stores[loc.Path] = store
	}

	st, err := state.Load(cfg.DataDir + "/state.json")
	if err != nil {
		return nil, fmt.Errorf("engine: loading state: %w", err)
	}

	probeStore, err := piecestore.New(cfg.DataDir + "/probe-scratch")
	if err != nil {
		return nil, fmt.Errorf("engine: opening probe scratch storage: %w", err)
	}

	httpClient := &http.Client{Timeout: 30 * time.Second}

	catalogURL := opts.CatalogURL
	if catalogURL == "" {
		catalogURL = atcatalog.DefaultURL
	}

	atBaseURL := opts.AcademicTorrentsBaseURL
	if atBaseURL == "" {
		atBaseURL = defaultAcademicTorrentsBaseURL
	}

	limiter := rate.NewLimiter(rate.Limit(cfg.Scan.RateLimitPerSecond), 1)

	e := &Engine{
		cfg:        cfg,
		logger:     logger,
		stores:     stores,
		probeStore: probeStore,
		state:      st,
		catalogFetcher: &atcatalog.Fetcher{
			CachePath:  cfg.DataDir + "/database.xml",
			HTTPClient: httpClient,
			UserAgent:  buildinfo.UserAgent(),
			URL:        catalogURL,
		},
		torrentFetcher: &attorrent.Fetcher{
			BaseURL:    atBaseURL,
			HTTPClient: httpClient,
			UserAgent:  buildinfo.UserAgent(),
			Limiter:    limiter,
		},
		httpClient:   httpClient,
		blocklist:    filter.NewKeywordBlocklist(cfg.KeywordBlocklist),
		probeTimeout: defaultProbeTimeout,
	}
	if opts.ProbeTimeout > 0 {
		e.probeTimeout = opts.ProbeTimeout
	}

	torrentClient, err := e.newTorrentClient(cfg)
	if err != nil {
		return nil, err
	}
	e.torrentClient = torrentClient

	if err := e.resumeHeldTorrents(); err != nil {
		return nil, fmt.Errorf("engine: resuming held torrents: %w", err)
	}

	return e, nil
}

func (e *Engine) newTorrentClient(cfg config.Config) (*torrent.Client, error) {
	tcfg := torrent.NewDefaultClientConfig()
	tcfg.ListenPort = cfg.Port
	tcfg.Seed = true
	tcfg.DataDir = cfg.DataDir
	tcfg.ExtendedHandshakeClientVersion = buildinfo.ExtendedHandshakeVersion()
	tcfg.Bep20 = buildinfo.PeerIDPrefix

	// Every torrent gets its storage assigned explicitly per-location in
	// addTorrentToClient, but the library still wants a default in case
	// something is added without one.
	if len(e.stores) > 0 {
		for _, s := range e.stores {
			tcfg.DefaultStorage = s
			break
		}
	}

	return torrent.NewClient(tcfg)
}

// networkStatsPath is where the current scan's network-wide stats are
// persisted, so a separate `keep-at network-status` invocation can read
// them without talking to this process directly.
func (e *Engine) networkStatsPath() string {
	return e.cfg.DataDir + "/network-stats.json"
}

// NetworkStatsPath returns the same path DataDir(cfg) would use, for
// callers (the CLI) that only have a Config, not a running Engine.
func NetworkStatsPath(cfg config.Config) string {
	return cfg.DataDir + "/network-stats.json"
}

// Close shuts down the BitTorrent client. It does not delete any data.
func (e *Engine) Close() error {
	if e.torrentClient != nil {
		errs := e.torrentClient.Close()
		if len(errs) > 0 {
			return fmt.Errorf("engine: closing torrent client: %v", errs)
		}
	}
	return nil
}

const defaultAcademicTorrentsBaseURL = "https://academictorrents.com"
