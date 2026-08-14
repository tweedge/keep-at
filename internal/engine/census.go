package engine

import (
	"bytes"
	"context"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"time"

	analog "github.com/anacrolix/log"
	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/metainfo"
	"golang.org/x/time/rate"

	"github.com/tweedge/keep-at/internal/atcatalog"
	"github.com/tweedge/keep-at/internal/attorrent"
	"github.com/tweedge/keep-at/internal/buildinfo"
	"github.com/tweedge/keep-at/internal/config"
	"github.com/tweedge/keep-at/internal/netstats"
	"github.com/tweedge/keep-at/internal/piecestore"
	"github.com/tweedge/keep-at/internal/selector"
)

// CensusOptions carries constructor inputs for RunCensus that aren't part
// of the user-facing YAML config - mainly test seams (overriding the
// catalog or Academic Torrents base URL) and the probe timeout.
type CensusOptions struct {
	CatalogURL              string
	AcademicTorrentsBaseURL string
	Logger                  *slog.Logger
	// ProbeTimeout is how long each individual swarm probe waits for peer
	// connections before giving up on that torrent.
	ProbeTimeout time.Duration
}

// CensusResult is the summary RunCensus produces once every catalog
// torrent has been scraped and probed.
type CensusResult struct {
	// CatalogSize is how many torrents the catalog lists.
	CatalogSize int

	// Scraped/Probed count torrents keep-at got a tracker scrape or a
	// swarm probe for, respectively. A torrent that failed metadata fetch
	// or scrape is skipped entirely (counted in Failed).
	Scraped int
	Probed  int
	Failed  int

	// The keep-at network census: distinct keep-at nodes observed, and the
	// total data those nodes were collectively seeding vs still
	// downloading. Deliberately not deduplicated across torrents for the
	// byte figures - the point is total keep-at-attributable capacity, see
	// netstats.Tracker's doc comment.
	NodeCount     int
	SeedingBytes  int64
	LeechingBytes int64

	// SeederFloor is the p10 seeder floor computed over this census's own
	// scrapes - the same figure scans persist, computed fresh here.
	SeederFloor int

	// Elapsed is how long the census took.
	Elapsed time.Duration
}

// CensusProgress is emitted periodically while a census runs, so `keep-at
// network-status` can print a running status line rather than sitting
// silent for the hours a full-catalog census takes.
type CensusProgress struct {
	Processed int
	Total     int
	NodeCount int
	Elapsed   time.Duration
}

// censusProbeTimeout is the default per-torrent swarm-probe wait when
// CensusOptions.ProbeTimeout isn't set. 10 seconds matches the old
// in-scan probe behavior.
const censusProbeTimeout = 10 * time.Second

// censusProgressEvery is how many torrents RunCensus probes between
// progress callbacks, so a status line appears roughly every
// censusProgressEvery * (scrape+probe time), not after every single
// torrent.
const censusProgressEvery = 25

// probePollInterval and probeMaxPeers bound how long a census probe waits
// to gather peer connections when probing a torrent's swarm for other
// keep-at nodes. There's no reason to wait the full timeout once enough
// peers have answered to make a reasonable estimate.
const (
	probePollInterval = 500 * time.Millisecond
	probeMaxPeers     = 8
)

// RunCensus runs a full network-status census: for every torrent in the
// catalog, in order, it fetches the torrent's metadata (cached where
// possible), scrapes AT's tracker for seeder counts (which feed the p10
// floor), and briefly joins the torrent's swarm to count other keep-at
// nodes - the "how many keep-at nodes are out there, and how much are they
// seeding" question that network-status answers.
//
// This is deliberately a separate, synchronous, operator-invoked operation
// (`keep-at network-status`), not part of the node's day-to-day scans:
// probing every catalog torrent is RAM- and time-heavy (it holds a
// torrent's worth of piece bookkeeping per probe, and each probe can wait
// up to the probe timeout for peers), and the node itself trusts the
// tracker's seeder counts for selection - it never probes swarms on its
// own. See DESIGN.md's network-status section.
//
// The probe client is created fresh for every single torrent (not
// accumulated and reset every 250 like the old in-scan probes), which
// keeps peak probe-client memory at roughly one torrent's bookkeeping
// instead of hundreds. That's safe because probes are strictly
// synchronous here: a probe fully finishes (or times out) before the
// client is torn down for the next torrent, so no in-flight probe ever
// races the reset. The old crashes happened with concurrent in-scan
// probes; the census has no concurrency to race with.
//
// progress, if non-nil, is called every censusProgressEvery torrents with
// a running tally.
func RunCensus(ctx context.Context, cfg config.Config, opts CensusOptions, progress func(CensusProgress)) (CensusResult, error) {
	logger := opts.Logger
	if logger == nil {
		logger = slog.Default()
	}

	if err := validateCensusConfig(cfg); err != nil {
		return CensusResult{}, fmt.Errorf("census: invalid config: %w", err)
	}

	httpClient := &http.Client{Timeout: 30 * time.Second}
	limiter := rate.NewLimiter(rate.Limit(cfg.Scan.RateLimitPerSecond), 1)

	catalogURL := opts.CatalogURL
	if catalogURL == "" {
		catalogURL = atcatalog.DefaultURL
	}
	atBaseURL := opts.AcademicTorrentsBaseURL
	if atBaseURL == "" {
		atBaseURL = defaultAcademicTorrentsBaseURL
	}

	catalogFetcher := &atcatalog.Fetcher{
		CachePath:  cfg.DataDir + "/database.xml",
		HTTPClient: httpClient,
		UserAgent:  buildinfo.UserAgent(),
		URL:        catalogURL,
	}
	torrentFetcher := &attorrent.Fetcher{
		BaseURL:    atBaseURL,
		HTTPClient: httpClient,
		UserAgent:  buildinfo.UserAgent(),
		Limiter:    limiter,
	}
	udpScraper := attorrent.NewUDPScraper()
	defer udpScraper.Close()

	// Resolve the operator's API key to the per-user announce URL, exactly
	// like the engine does, so probed torrents announce to AT's tracker
	// with the operator's passkey. Non-fatal.
	userAnnounceURL, userAnnounceIPv6URL := "", ""
	if cfg.APIKey != "" {
		resolveCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
		ua, ua6, err := resolveUserAnnounce(resolveCtx, httpClient, cfg.APIKey)
		cancel()
		if err != nil {
			logger.Warn("census: could not resolve API key to a user announce URL", "err", err)
		} else {
			userAnnounceURL, userAnnounceIPv6URL = ua, ua6
		}
	}

	// The probe client's own scratch storage. Probing never reads or writes
	// real data (DisallowDataDownload/Upload), but anacrolix still wants a
	// storage backend for the torrent it joins. Reuse the engine's
	// probe-scratch directory so a census after a scan doesn't create a
	// fresh one.
	probeStore, err := piecestore.New(cfg.DataDir + "/probe-scratch")
	if err != nil {
		return CensusResult{}, fmt.Errorf("census: opening probe scratch storage: %w", err)
	}

	probeTimeout := opts.ProbeTimeout
	if probeTimeout <= 0 {
		probeTimeout = censusProbeTimeout
	}

	catalog, err := catalogFetcher.Load(ctx, cfg.Scan.Interval.AsDuration())
	if err != nil && len(catalog.Items) == 0 {
		return CensusResult{}, fmt.Errorf("census: loading catalog: %w", err)
	}
	if err != nil {
		logger.Warn("census: catalog refresh failed, continuing with stale cache", "err", err)
	}

	logger.Info("census: catalog loaded", "items", len(catalog.Items))

	// Shuffle the catalog first so the census doesn't front-load whatever
	// cluster database.xml happens to list first - the same reason scans
	// shuffle (see ScanOnce).
	shuffleCatalogItems(catalog.Items)

	startedAt := time.Now()
	tracker := netstats.NewTracker()
	seederCounts := make([]int, 0, len(catalog.Items))

	result := CensusResult{CatalogSize: len(catalog.Items)}
	processed := 0

	for _, item := range catalog.Items {
		if ctx.Err() != nil {
			return CensusResult{}, fmt.Errorf("census: interrupted: %w", ctx.Err())
		}

		md, err := fetchCachedMetadata(ctx, cfg.DataDir, torrentFetcher, logger, item.InfoHash)
		if err != nil {
			result.Failed++
			logger.Debug("census: could not fetch metadata", "title", item.Title, "err", err)
			processed++
			reportCensusProgress(progress, startedAt, processed, result.CatalogSize, tracker)
			continue
		}

		sizeBytes := md.Info.TotalLength()

		// Tracker scrape: this feeds the p10 seeder floor, computed fresh
		// over the census's own scrapes below.
		swarm, err := scrapeSwarmWith(ctx, limiter, httpClient, udpScraper, md.Trackers, item.InfoHash)
		if err != nil {
			result.Failed++
			logger.Debug("census: could not scrape trackers", "title", item.Title, "err", err)
			processed++
			reportCensusProgress(progress, startedAt, processed, result.CatalogSize, tracker)
			continue
		}
		result.Scraped++
		seederCounts = append(seederCounts, swarm.Seeders)

		// Probe the swarm for other keep-at nodes using a fresh probe
		// client, then tear it down immediately: per-torrent client
		// lifecycle means per-torrent peak memory, and the synchronous
		// probe below makes the teardown race-free (see RunCensus's doc
		// comment).
		probeClient, err := newProbeClient(cfg, logger, limiter, probeStore)
		if err != nil {
			return CensusResult{}, fmt.Errorf("census: creating probe client: %w", err)
		}

		observed, probeErr := probeSwarmOn(ctx, probeClient, probeStore, md.MetaInfo, userAnnounceURL, userAnnounceIPv6URL, probeTimeout)

		// Tear down the client before handling the result: the probe has
		// fully returned (or timed out), so nothing is in flight against
		// it. This is the whole point of the per-torrent lifecycle - the
		// accumulated per-piece bookkeeping of every torrent this census
		// probes is released here, not held for the rest of the run.
		probeClient.Close()

		if probeErr != nil {
			result.Failed++
			logger.Debug("census: could not probe swarm", "title", item.Title, "err", probeErr)
		} else {
			result.Probed++
			for _, obs := range observed {
				tracker.Observe(obs.nodeKey, sizeBytes, obs.complete)
			}
		}

		processed++
		reportCensusProgress(progress, startedAt, processed, result.CatalogSize, tracker)
	}

	result.NodeCount = tracker.NodeCount()
	result.SeedingBytes = tracker.SeedingBytes()
	result.LeechingBytes = tracker.LeechingBytes()
	result.SeederFloor = selector.SeederFloor(seederCounts)
	result.Elapsed = time.Since(startedAt)

	return result, nil
}

// validateCensusConfig checks the small config surface a census actually
// uses. It deliberately does NOT require storage locations (the census
// stores nothing) or the engine's full validation - it only needs a data
// dir and a working rate limit.
func validateCensusConfig(cfg config.Config) error {
	if cfg.DataDir == "" {
		return fmt.Errorf("data_dir must not be empty")
	}
	if cfg.Scan.RateLimitPerSecond <= 0 {
		return fmt.Errorf("scan.rate_limit_per_second must be positive")
	}
	return nil
}

// reportCensusProgress invokes progress on the censusProgressEvery cadence.
func reportCensusProgress(progress func(CensusProgress), startedAt time.Time, processed, total int, tracker *netstats.Tracker) {
	if progress == nil || processed%censusProgressEvery != 0 {
		return
	}
	progress(CensusProgress{Processed: processed, Total: total, NodeCount: tracker.NodeCount(), Elapsed: time.Since(startedAt)})
}

// newProbeClient builds the census's probe torrent client: DHT disabled
// (not needed to answer "who else is in this swarm right now"), uTP
// disabled (TCP peers are what a probe wants, and the uTP packet reader is
// pure CPU), no data transfer allowed, and announcing only to AT's tracker
// through the shared rate limiter.
func newProbeClient(cfg config.Config, logger *slog.Logger, limiter *rate.Limiter, probeStore *piecestore.Client) (*torrent.Client, error) {
	tcfg := torrent.NewDefaultClientConfig()
	tcfg.ListenPort = 0 // ephemeral
	tcfg.Seed = true
	tcfg.DataDir = cfg.DataDir
	// The scraper role suffix is what lets other keep-at nodes tell a
	// census prober apart from a real seeder (see
	// buildinfo.IsKeepAtSeeder) - a census must never be counted as a
	// keep-at seeder by the nodes it's probing.
	tcfg.ExtendedHandshakeClientVersion = buildinfo.ScraperExtendedHandshakeVersion()
	tcfg.HTTPUserAgent = buildinfo.ScraperUserAgent()
	tcfg.Bep20 = buildinfo.PeerIDPrefix
	tcfg.NoDHT = true
	tcfg.DisableUTP = true

	tcfg.EstablishedConnsPerTorrent = establishedConnsPerTorrent
	tcfg.HalfOpenConnsPerTorrent = halfOpenConnsPerTorrent
	tcfg.MaxAllocPeerRequestDataPerConn = maxAllocPeerRequestData
	tcfg.TotalHalfOpenConns = totalHalfOpenConns

	tcfg.TrackerDialContext = func(ctx context.Context, network, addr string) (net.Conn, error) {
		return rateLimitedTrackerDialer(limiter, ctx, network, addr)
	}
	tcfg.Logger = analog.Default.WithFilterLevel(analog.Error)
	tcfg.DisableWebseeds = true
	tcfg.DefaultStorage = probeStore

	return torrent.NewClient(tcfg)
}

// probeSwarmOn adds mi to probeClient, waits up to timeout for peer
// connections, and returns one observation per connected peer that
// self-identifies as a keep-at seeder. It's the census's equivalent of the
// old in-scan probeSwarm, parameterized so it needs no Engine.
func probeSwarmOn(ctx context.Context, probeClient *torrent.Client, probeStore *piecestore.Client, mi *metainfo.MetaInfo, userAnnounceURL, userAnnounceIPv6URL string, timeout time.Duration) ([]peerObservation, error) {
	spec, err := torrent.TorrentSpecFromMetaInfoErr(mi)
	if err != nil {
		return nil, err
	}
	spec.Storage = probeStore
	spec.DisallowDataDownload = true
	spec.DisallowDataUpload = true
	// Announce only to AT's tracker (keyed with the operator's URL when
	// configured), never to the dead third-party trackers the catalog's
	// .torrent files list - same reasoning as addTorrentSpec.
	spec.Trackers = atTrackersOnly(spec.Trackers, userAnnounceURL, userAnnounceIPv6URL)

	t, _, err := probeClient.AddTorrentSpec(spec)
	if err != nil {
		return nil, err
	}

	deadline := time.Now().Add(timeout)
	ticker := time.NewTicker(probePollInterval)
	defer ticker.Stop()

	for {
		if observed := keepAtPeers(t); len(observed) > 0 || len(t.PeerConns()) >= probeMaxPeers {
			return observed, nil
		}
		if time.Now().After(deadline) {
			return keepAtPeers(t), nil
		}
		select {
		case <-ctx.Done():
			return keepAtPeers(t), ctx.Err()
		case <-ticker.C:
		}
	}
}

// scrapeSwarmWith is scrapeSwarm parameterized for the census, which has
// no Engine.
func scrapeSwarmWith(ctx context.Context, limiter *rate.Limiter, httpClient *http.Client, udpScraper *attorrent.UDPScraper, trackers []string, infoHash metainfo.Hash) (attorrent.SwarmCounts, error) {
	var lastErr error
	for _, trackerURL := range trackers {
		if isAcademicTorrentsHost(trackerURL) && limiter != nil {
			if err := limiter.Wait(ctx); err != nil {
				return attorrent.SwarmCounts{}, err
			}
		}

		callCtx, cancel := context.WithTimeout(ctx, scrapeTimeout)
		counts, err := attorrent.Scrape(callCtx, httpClient, udpScraper, buildinfo.ScraperUserAgent(), trackerURL, []metainfo.Hash{infoHash})
		cancel()
		if err != nil {
			lastErr = err
			continue
		}
		if c, ok := counts[infoHash]; ok {
			return c, nil
		}
	}
	if lastErr == nil {
		lastErr = fmt.Errorf("census: no tracker returned scrape data for %s", infoHash.HexString())
	}
	return attorrent.SwarmCounts{}, lastErr
}

// fetchCachedMetadata is fetchMetadata without an Engine: reads the
// torrent-cache dir under dataDir, falling back to torrentFetcher.
func fetchCachedMetadata(ctx context.Context, dataDir string, torrentFetcher *attorrent.Fetcher, logger *slog.Logger, infoHash metainfo.Hash) (*attorrent.Metadata, error) {
	path := filepath.Join(dataDir, "torrent-cache", infoHash.HexString()+".torrent")
	if data, err := os.ReadFile(path); err == nil {
		if md, parseErr := attorrent.ParseTorrentBytes(data); parseErr == nil {
			return md, nil
		}
	}
	md, err := torrentFetcher.FetchTorrent(ctx, infoHash)
	if err != nil {
		return nil, err
	}
	// Best-effort cache write, like the engine's saveMetadataCache.
	if err := saveCachedMetadata(dataDir, infoHash, md); err != nil {
		logger.Warn("census: failed to cache fetched torrent file", "infohash", infoHash.HexString(), "err", err)
	}
	return md, nil
}

func saveCachedMetadata(dataDir string, infoHash metainfo.Hash, md *attorrent.Metadata) error {
	path := filepath.Join(dataDir, "torrent-cache", infoHash.HexString()+".torrent")
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	var buf bytes.Buffer
	if err := md.MetaInfo.Write(&buf); err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, buf.Bytes(), 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}
