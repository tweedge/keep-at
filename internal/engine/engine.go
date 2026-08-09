// Package engine ties every other package together into keep-at's actual
// runtime behavior: periodically scanning the Academic Torrents catalog,
// deciding what to seed, and running the underlying BitTorrent client.
package engine

import (
	"context"
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

// Connection settings for the underlying anacrolix/torrent client. These are
// kept deliberately modest: keep-at is a seed-first, always-on daemon meant to
// run on everything from a 1 GB Raspberry Pi to a big server, and Academic
// Torrents is seeded by many keep-at nodes in aggregate - so no single node
// needs to be a speed monster or lean on a huge per-torrent peer fanout.
//
// Critically, anacrolix/torrent's peer-connection buffer pool is per-torrent
// (each held torrent keeps up to EstablishedConnsPerTorrent independent peer
// connections, each buffering up to MaxAllocPeerRequestDataPerConn), so these
// values directly set how much RAM each held torrent costs - which is exactly
// what the RAM budget (see ram.go) uses to decide how many torrents fit.
const (
	establishedConnsPerTorrent   = 12
	halfOpenConnsPerTorrent      = 6
	maxAllocPeerRequestData      = 256 << 10 // 256 KiB
	totalHalfOpenConns           = 40
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
	swarmCache *swarmCache                   // persisted per-torrent scrape counts, reused across scans

	// userAnnounceURL (and its ipv6 variant) is the per-user Academic
	// Torrents announce URL resolved from cfg.APIKey at startup. When set,
	// addTorrentSpec/probeSwarm swap AT tracker URLs for it, so AT attributes
	// kept torrents to the operator's account. Empty when no API key is
	// configured or resolution failed. The URL contains the account's
	// passkey and must never be logged or written to disk.
	userAnnounceURL     string
	userAnnounceIPv6URL string

	// maxTorrents is the hard cap on how many torrents keep-at will hold at
	// once, derived from the RAM budget (see ram.go). RAM scales per-torrent
	// in anacrolix/torrent, so capping the count is what bounds memory use.
	maxTorrents int

	// startedAt is when this Engine was constructed, the reference point
	// for the "since boot" transfer and uptime figures in RuntimeStats.
	startedAt time.Time

	// lastScanStats is the most recent scan's counters (see scanStats),
	// retained after ScanOnce returns so tests and callers can assert on
	// what the scan actually did. Guarded by the same lock that serializes
	// scans (Run runs one scan at a time), so no extra synchronization.
	lastScanStats *scanStats

	catalogFetcher *atcatalog.Fetcher
	torrentFetcher *attorrent.Fetcher
	httpClient     *http.Client
	udpScraper     *attorrent.UDPScraper
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

	// Resolve `limit: all` storage locations to concrete byte limits (a safe
	// fraction of the device's total formatted capacity) before anything
	// that consults limits runs. Returns a copy; cfg keeps the operator's
	// "all" for anything that round-trips config back to disk.
	resolved, err := resolveAllLimits(cfg, logger)
	if err != nil {
		return nil, fmt.Errorf("engine: %w", err)
	}
	cfg = resolved

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

	swarmCache := newSwarmCache(cfg.DataDir+"/scrape-cache.json", cfg.Scan.Interval.AsDuration())
	if err := swarmCache.load(); err != nil {
		// A corrupt cache is non-fatal: keep-at just re-scrapes everything
		// this scan. Log and continue with an empty cache.
		logger.Warn("could not load swarm cache, starting fresh", "err", err)
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

	// Resolve the operator's API key (if set) to the per-user announce URL
	// up front. This is one request to Academic Torrents' own endpoint, sent
	// with the key as cookies; the resulting URL carries the account's
	// passkey and is what makes AT show this operator as hosting whatever
	// keep-at seeds. Failure is non-fatal: keep-at still runs, just without
	// account attribution (the key itself is only ever used for this
	// resolution and the keyed announces below).
	userAnnounceURL, userAnnounceIPv6URL := "", ""
	if cfg.APIKey != "" {
		resolveCtx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		ua, ua6, err := resolveUserAnnounce(resolveCtx, httpClient, cfg.APIKey)
		cancel()
		if err != nil {
			logger.Warn("could not resolve Academic Torrents API key to a user announce URL; seeded torrents won't be attributed to your account",
				"err", err)
		} else {
			userAnnounceURL, userAnnounceIPv6URL = ua, ua6
			logger.Info("Academic Torrents API key resolved; seeded torrents will be attributed to your account")
		}
	}

	e := &Engine{
		cfg:        cfg,
		logger:     logger,
		stores:     stores,
		probeStore: probeStore,
		state:      st,
		swarmCache: swarmCache,
		startedAt:  time.Now(),
		userAnnounceURL:     userAnnounceURL,
		userAnnounceIPv6URL: userAnnounceIPv6URL,
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
		udpScraper:   attorrent.NewUDPScraper(),
		blocklist:    filter.NewKeywordBlocklist(cfg.KeywordBlocklist),
		probeTimeout: defaultProbeTimeout,
	}
	if opts.ProbeTimeout > 0 {
		e.probeTimeout = opts.ProbeTimeout
	}

	// Work out keep-at's RAM budget before spinning up the torrent client,
	// so the torrent-count cap (the thing that actually bounds memory, since
	// anacrolix/torrent's footprint scales per-torrent) is wired in from the
	// start. The per-torrent footprint uses the exact connection settings
	// declared above.
	systemTotal, sysErr := SystemTotalRAM()
	if sysErr != nil {
		e.logger.Warn("could not measure system RAM; the RAM-driven torrent cap is disabled", "err", sysErr)
	}
	perTorrent := PerTorrentConnRAM(establishedConnsPerTorrent, maxAllocPeerRequestData) + PerTorrentRAMBase
	budget, hardCap, maxTorrents := ramBudget(systemTotal, int64(cfg.MaxRAM), int64(cfg.MaxRAMConfig), perTorrent)
	e.maxTorrents = maxTorrents

	// The operator's explicit --max-ram (or config max_ram) must never ask
	// for more than the hard 80%-of-system cap. ramBudget already clamps the
	// effective budget, but we reject out of hand so a misconfiguration
	// fails loud rather than silently running against the cap. When system
	// RAM can't be measured (systemTotal == 0), keep-at applies no cap and
	// there's nothing to reject against.
	if sysErr == nil && systemTotal > 0 {
		if cfg.MaxRAM > 0 && int64(cfg.MaxRAM) > hardCap {
			return nil, fmt.Errorf("engine: max_ram %s exceeds the 80%%-of-system hard cap of %s (system total %s)",
				cfg.MaxRAM.String(), config.ByteSize(hardCap).String(), config.ByteSize(systemTotal).String())
		}
		if cfg.MaxRAMConfig > 0 && int64(cfg.MaxRAMConfig) > hardCap {
			return nil, fmt.Errorf("engine: config max_ram %s exceeds the 80%%-of-system hard cap of %s (system total %s)",
				cfg.MaxRAMConfig.String(), config.ByteSize(hardCap).String(), config.ByteSize(systemTotal).String())
		}
	}

	e.logger.Info("RAM budget",
		"system_total", humanBytes(systemTotal),
		"hard_cap_80pct", humanBytes(hardCap),
		"budget", humanBytes(budget),
		"per_torrent_footprint", humanBytes(perTorrent),
		"max_torrents", maxTorrents)

	torrentClient, err := e.newTorrentClient(cfg.Port, false, false)
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

func (e *Engine) newTorrentClient(listenPort int, noDHT bool, scraper bool) (*torrent.Client, error) {
	tcfg := torrent.NewDefaultClientConfig()
	tcfg.ListenPort = listenPort
	tcfg.Seed = true
	tcfg.DataDir = e.cfg.DataDir
	// The extended handshake "v" string is what other keep-at nodes read
	// (see keepAtPeers) and the HTTP User-Agent is what AT's tracker records
	// on announces and shows on its Technical pages. Both carry keep-at plus
	// the version, and a role suffix so the probe (scraper) client is
	// distinguishable from the main (seeder) client - see buildinfo.
	if scraper {
		tcfg.ExtendedHandshakeClientVersion = buildinfo.ScraperExtendedHandshakeVersion()
		tcfg.HTTPUserAgent = buildinfo.ScraperUserAgent()
	} else {
		tcfg.ExtendedHandshakeClientVersion = buildinfo.ExtendedHandshakeVersion()
		tcfg.HTTPUserAgent = buildinfo.SeederUserAgent()
	}
	tcfg.Bep20 = buildinfo.PeerIDPrefix
	tcfg.NoDHT = noDHT

	// Optional upload/download throttling. Zero means unlimited, which is
	// what the library defaults to, so a limiter is only wired in when the
	// operator actually set one. The burst is left at zero and filled in by
	// the library (see ClientConfig.setRateLimiterBursts) so it stays
	// consistent with keep-at's other connection settings.
	if bytesPerSec := int64(e.cfg.UploadRateLimit); bytesPerSec > 0 {
		tcfg.UploadRateLimiter = rate.NewLimiter(rate.Limit(bytesPerSec), 0)
	}
	if bytesPerSec := int64(e.cfg.DownloadRateLimit); bytesPerSec > 0 {
		tcfg.DownloadRateLimiter = rate.NewLimiter(rate.Limit(bytesPerSec), 0)
	}

	// Memory tuning. The library defaults are sized for a handful of
	// torrents (EstablishedConnsPerTorrent=50, MaxAllocPeerRequestDataPerConn=1MiB),
	// so a seeding daemon holding hundreds of torrents buffers up to
	// roughly heldTorrents * 50 * 1MiB of upload data on warm peer
	// connections alone - easily multiple GB and the dominant RAM cost.
	// keep-at is a seed-first daemon, so it never needs a huge per-torrent
	// peer fanout; a smaller cap keeps peer connection memory bounded and
	// spreads it predictably across held torrents instead of scaling with
	// the whole catalog. These are applied to the probe client too, where
	// they don't hurt swarm-probing accuracy but prevent the second client
	// from doubling the connection footprint. The values are the named
	// constants declared above so the RAM budget (ram.go) can compute the
	// per-torrent footprint from exactly the same numbers.
	tcfg.EstablishedConnsPerTorrent = establishedConnsPerTorrent
	tcfg.HalfOpenConnsPerTorrent = halfOpenConnsPerTorrent
	tcfg.MaxAllocPeerRequestDataPerConn = maxAllocPeerRequestData
	tcfg.TotalHalfOpenConns = totalHalfOpenConns

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

	// keep-at seeds to and downloads from real BitTorrent peers; webseeds
	// (HTTP/FTP fallback sources some .torrent files list) are never
	// relied on. Disabling them isn't just tidiness: a real full-catalog
	// scan hit a nil pointer dereference deep in anacrolix/torrent's
	// webseed request machinery (a background timer callback, so nothing
	// in keep-at's own code could have recovered from it), and
	// DisableWebseeds stops the library from ever creating a webseed peer
	// in the first place, which avoids that code path entirely.
	tcfg.DisableWebseeds = true

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
	newClient, err := e.newTorrentClient(0, true, true)
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

// Close shuts down both BitTorrent clients and the cached UDP tracker
// connections. It does not delete any data.
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
	if e.udpScraper != nil {
		if err := e.udpScraper.Close(); err != nil {
			errs = append(errs, err)
		}
	}
	if len(errs) > 0 {
		return fmt.Errorf("engine: closing torrent clients: %v", errs)
	}
	return nil
}

const defaultAcademicTorrentsBaseURL = "https://academictorrents.com"
