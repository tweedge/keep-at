// Package engine ties every other package together into keep-at's actual
// runtime behavior: periodically scanning the Academic Torrents catalog,
// deciding what to seed, and running the underlying BitTorrent client.
package engine

import (
	"fmt"
	"log/slog"
	"net/http"
	"sync"
	"time"

	analog "github.com/anacrolix/log"
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

	// probeTorrentClient is used only for swarm probing (see probe.go) and
	// is recreated wholesale at the start of every scan (resetProbeClient)
	// rather than having individual torrents dropped from it - see that
	// method's comment for why. probeClientMu guards reassignment against
	// the concurrent probes that read it during evaluateCandidates.
	probeClientMu      sync.RWMutex
	probeTorrentClient *torrent.Client

	stores     map[string]*piecestore.Client // keyed by storage location path
	probeStore *piecestore.Client            // scratch storage for swarm-probing candidates, see probe.go
	state      *state.State

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

	torrentClient, err := e.newTorrentClient(cfg.Port, false)
	if err != nil {
		return nil, err
	}
	e.torrentClient = torrentClient

	// A second, DHT-disabled client used only for swarm probing - see
	// resetProbeClient for why it's DHT-disabled and recreated wholesale
	// rather than having individual torrents dropped from it.
	if err := e.resetProbeClient(); err != nil {
		return nil, fmt.Errorf("engine: starting probe torrent client: %w", err)
	}

	if err := e.resumeHeldTorrents(); err != nil {
		return nil, fmt.Errorf("engine: resuming held torrents: %w", err)
	}

	return e, nil
}

func (e *Engine) newTorrentClient(listenPort int, noDHT bool) (*torrent.Client, error) {
	tcfg := torrent.NewDefaultClientConfig()
	tcfg.ListenPort = listenPort
	tcfg.Seed = true
	tcfg.DataDir = e.cfg.DataDir
	tcfg.ExtendedHandshakeClientVersion = buildinfo.ExtendedHandshakeVersion()
	tcfg.Bep20 = buildinfo.PeerIDPrefix
	tcfg.NoDHT = noDHT

	// The torrent library re-announces to every tracker in a torrent's
	// spec on its own schedule, independent of scrapeSwarm/attorrent's
	// rate limiting. Routing tracker dials through the same limiter keeps
	// that automatic traffic to academictorrents.com under the same
	// budget as keep-at's own requests - see rateLimitedTrackerDialer.
	tcfg.TrackerDialContext = e.rateLimitedTrackerDialer

	// Academic Torrents' catalog spans over a decade of uploads: plenty of
	// torrents have malformed webseed URLs or long-dead third-party
	// trackers, and anacrolix/torrent logs a warning for every single
	// failed attempt against either. At full-catalog scale that's an
	// overwhelming volume of expected, non-actionable noise. Setting
	// ClientConfig.Logger's filter level (not Slogger - the library's
	// bridge from its own Logger to Slogger doesn't reliably route every
	// internal warning through a wrapped Slogger, verified empirically)
	// is what actually suppresses it. keep-at's own logging (scan
	// progress, its own request failures) is unaffected - this only
	// silences the underlying library's internal chatter.
	tcfg.Logger = analog.Default.WithFilterLevel(analog.Error)

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

// resetProbeClient closes any existing probe torrent client and starts a
// fresh one. Called once at Engine startup and again at the start of
// every scan (see ScanOnce).
//
// Probing works by adding a torrent to this client just long enough to
// see who's in its swarm, then - in earlier versions of this code -
// dropping it again immediately. anacrolix/torrent v1.61.0 has more than
// one internal race between a torrent's background per-torrent goroutines
// (DHT announcing, regular tracker announcing) and that same torrent
// being dropped or re-added in quick succession; both surfaced as a fatal,
// unrecoverable "sync: Unlock of unlocked RWMutex" that killed the whole
// process, not just the affected torrent, during a real full-catalog scan.
//
// Rather than drop torrents individually (and keep finding new variations
// of this race), probed torrents are simply never dropped - they
// accumulate in this client for the rest of the current scan, which is
// harmless since they hold no real data and never attempt one. The whole
// client is discarded and replaced at the start of the next scan, which
// releases everything at once through a code path that doesn't share the
// per-torrent add/drop race. See DESIGN.md.
//
// DHT stays disabled specifically on this client (unlike the main one)
// because DHT's own per-torrent announce goroutine was one of the two
// concrete triggers found for this bug, and DHT isn't needed to answer
// "who else is in this swarm right now" - regular trackers are enough for
// that, and this client's torrents are never kept around long enough for
// DHT's slower discovery to matter anyway.
func (e *Engine) resetProbeClient() error {
	e.probeClientMu.Lock()
	defer e.probeClientMu.Unlock()

	old := e.probeTorrentClient
	newClient, err := e.newTorrentClient(0, true)
	if err != nil {
		return err
	}
	e.probeTorrentClient = newClient

	if old != nil {
		// Safe to close synchronously: this is only ever called once the
		// previous scan's evaluateCandidates has fully returned (all of
		// its probes finished), or at Engine startup when there's no old
		// client at all.
		old.Close()
	}
	return nil
}

func (e *Engine) currentProbeClient() *torrent.Client {
	e.probeClientMu.RLock()
	defer e.probeClientMu.RUnlock()
	return e.probeTorrentClient
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

// Close shuts down both BitTorrent clients. It does not delete any data.
func (e *Engine) Close() error {
	var errs []error
	if e.torrentClient != nil {
		for _, err := range e.torrentClient.Close() {
			errs = append(errs, err)
		}
	}
	if probeClient := e.currentProbeClient(); probeClient != nil {
		for _, err := range probeClient.Close() {
			errs = append(errs, err)
		}
	}
	if len(errs) > 0 {
		return fmt.Errorf("engine: closing torrent clients: %v", errs)
	}
	return nil
}

const defaultAcademicTorrentsBaseURL = "https://academictorrents.com"
