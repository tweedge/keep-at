package engine

import (
	"context"
	"fmt"
	"math/rand"
	"sort"
	"sync"
	"sync/atomic"
	"time"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/atcatalog"
	"github.com/tweedge/keep-at/internal/attorrent"
	"github.com/tweedge/keep-at/internal/filter"
	"github.com/tweedge/keep-at/internal/netstats"
	"github.com/tweedge/keep-at/internal/selector"
	"github.com/tweedge/keep-at/internal/state"
)

// evaluateConcurrency bounds how many candidates keep-at evaluates (and,
// for available ones, probes the swarm of) at once. Fetching a torrent
// file and scraping a tracker are still globally rate-limited against
// Academic Torrents regardless of this (see attorrent.Fetcher.Limiter) -
// this only parallelizes the part of evaluation that isn't AT-bound, most
// importantly the swarm probe, which can wait several seconds per
// candidate for peer connections. Run sequentially, a catalog with a few
// thousand available candidates would take hours; run with this much
// concurrency, it's minutes.
const evaluateConcurrency = 16

// progressSaveInterval is how often ScanOnce persists a network-status
// snapshot while candidates are being evaluated concurrently, rather than
// after every single one (which would need its own synchronization for
// no real benefit - nothing is watching that closely).
const progressSaveInterval = 2 * time.Second

// progressLogInterval is how often ScanOnce writes a human-readable
// progress line (with an ETA) to the log while scraping. This is
// deliberately much coarser than progressSaveInterval: that one just
// writes a small JSON file for `network-status` to read on demand, while
// this one is meant to reassure an operator watching the log that a
// scrape that can run for a long time on a large catalog is still making
// progress, without flooding the log while it does.
const progressLogInterval = 5 * time.Minute

// probeClientResetInterval bounds how many candidates' worth of probed
// torrents accumulate in the probe client (see resetProbeClient) before
// it gets discarded and replaced mid-scan, rather than only once at the
// very start of the next scan.
//
// resetProbeClient's whole premise is that probed torrents are cheap to
// leave sitting in memory since they hold no real data - true for a
// typical torrent, but not for Academic Torrents specifically: some
// datasets are large enough that just the piece-level bookkeeping for one
// torrent (hashes and per-piece state, scaling with piece count, not with
// how much of it keep-at has actually downloaded - which for a probe is
// nothing) runs into tens of megabytes. Verified in a real full-catalog
// run: memory usage grew roughly linearly with candidates processed,
// on track to exceed available RAM well before a scan of the full ~2,850
// item catalog would finish. Resetting every 250 candidates instead of
// every ~2,850 bounds peak memory to roughly what that many candidates'
// probes need, at the cost of a probe that was still in flight against
// the client being reset occasionally erroring out - already handled the
// same as any other probe failure (logged, treated as zero peers
// observed for that one candidate).
const probeClientResetInterval = 250

// evaluatedCandidate is a catalog item keep-at has fetched metadata and a
// fresh scrape for, and is ready to rank and possibly act on. The swarm
// probe (which counts other keep-at nodes for the anti-cascade check) is
// deliberately NOT done here - it waits several seconds per candidate and is
// only needed at the moment of decision, so tryAdd probes on demand for just
// the candidates keep-at is actually about to act on.
//
// It deliberately carries only the lightweight facts ranking needs (title,
// infohash, size, swarm counts) rather than the full parsed .torrent
// metadata. The full metadata - whose piece-hash arrays are the dominant
// memory cost of a scan - is cached to disk in torrent-cache by
// fetchMetadata and is re-read from there only when keep-at actually acts on
// a candidate (see actOnCandidate). This keeps a full-catalog scan's memory
// footprint proportional to the number of candidates, not to the total size
// of the library, which is what makes keep-at usable on a 1GB-RAM device.
type evaluatedCandidate struct {
	title     string
	infoHash  metainfo.Hash
	sizeBytes int64
	swarm     attorrent.SwarmCounts
}

// scanStats accumulates what a single scan actually did, so the completion
// log can say how much of the work hit Academic Torrents versus keep-at's
// caches, how many candidates were skipped (and why), and how big the
// catalog is. All fields are atomic because evaluation runs concurrently.
type scanStats struct {
	// LibraryBytes is the summed size of every catalog item, so the log can
	// report how big the Academic Torrents library is.
	libraryBytes int64

	// Metadata fetches: one .torrent GET per uncached candidate, vs served
	// from keep-at's on-disk torrent cache.
	metadataFetched atomic.Int64
	metadataCached  atomic.Int64

	// Tracker scrapes: one per candidate vs counts reused from keep-at's
	// swarm cache. (AT's tracker does not support batched multi-hash scrapes,
	// so keep-at scrapes one candidate per request.)
	scrapeRequests atomic.Int64
	scrapeCached   atomic.Int64

	// Candidates skipped entirely, by reason.
	skippedHeld      atomic.Int64
	skippedBlocked   atomic.Int64
	skippedAge       atomic.Int64
	skippedFetchErr  atomic.Int64
	skippedScrapeErr atomic.Int64

	// Candidates that made it through evaluation (eligible to be added).
	eligible atomic.Int64
}

// ScanOnce runs one full pass: refresh the catalog, drop anything Academic
// Torrents has taken down, refresh seed counts for what keep-at already
// holds, and then look for new torrents to fill free space or displace
// lower-priority ones. It's expected to take a while on a large catalog,
// and is meant to be called periodically, not continuously. Progress and
// network-wide stats gathered along the way are persisted incrementally so
// `keep-at network-status` can report on a scan that's still running.
func (e *Engine) ScanOnce(ctx context.Context) error {
	scanStartedAt := time.Now().UTC()
	e.saveNetworkStats(netstats.Snapshot{ScanStartedAt: scanStartedAt})

	// Fresh probe client per scan - see resetProbeClient for why this
	// matters for stability, not just tidiness.
	if err := e.resetProbeClient(); err != nil {
		return fmt.Errorf("engine: resetting probe client: %w", err)
	}

	catalog, err := e.catalogFetcher.Load(ctx, e.cfg.Scan.Interval.AsDuration())
	if err != nil && len(catalog.Items) == 0 {
		return err
	}
	if err != nil {
		e.logger.Warn("catalog refresh failed, continuing with stale cache", "err", err)
	}
	e.logger.Info("catalog loaded", "items", len(catalog.Items))

	catalogHashes := make(map[string]bool, len(catalog.Items))
	for _, item := range catalog.Items {
		catalogHashes[item.InfoHash.HexString()] = true
	}

	held := e.state.All()
	heldHashes := make(map[string]bool, len(held))
	for _, h := range held {
		heldHashes[h.InfoHash.HexString()] = true
	}

	e.removeDeletedTorrents(held, catalogHashes)
	e.refreshHeldSeederCounts(ctx, held, catalogHashes)

	totalCandidates := countPendingCandidates(catalog, heldHashes, e.blocklist)
	e.saveNetworkStats(netstats.Snapshot{ScanStartedAt: scanStartedAt, TotalCandidates: totalCandidates})
	e.logger.Info("starting scrape: fetching torrent metadata and tracker data for every pending catalog candidate to work out what needs seeding most - this can take a while on a large catalog, and downloads start gradually as the highest-priority candidates are found rather than waiting for the whole scrape to finish",
		"total", totalCandidates)

	stats := &scanStats{}
	for _, item := range catalog.Items {
		stats.libraryBytes += item.SizeBytes
	}

	tracker := netstats.NewTracker()
	scrapeStartedAt := time.Now()
	candChan := e.evaluateCandidates(ctx, catalog, heldHashes, tracker, scanStartedAt, totalCandidates, stats)

	// Incremental acting: evaluate candidates stream in, and we act on the
	// highest-priority ones as soon as they're known, rather than waiting
	// for the whole catalog. This is what stops free disk from sitting idle
	// for hours on a first scan - the top candidates start seeding within
	// minutes. We only ever act on a candidate while it's still in the
	// running top window (see actOnWindowed), so we never seed something
	// that isn't genuinely among the best keep-at could hold.
	var evaluated []evaluatedCandidate
	acted := make(map[string]bool)
	heldCount := len(held)
	processed := 0
	ramBound := heldCount >= e.maxTorrents

	// Act incrementally, but not on every single candidate. actOnWindowed
	// re-ranks everything evaluated so far each time it's called, so acting
	// per-candidate would make ranking O(N^2 log N) across a full-catalog
	// scan. Evaluating and acting once per evaluateConcurrency arrivals keeps
	// the "top candidates start seeding within minutes" property while
	// keeping ranking work proportional to N * log N (plus one final flush
	// for whatever the last partial batch leaves).
	actEvery := evaluateConcurrency
	for c := range candChan {
		processed++
		evaluated = append(evaluated, c)
		if processed%actEvery == 0 {
			ramBound = heldCount >= e.maxTorrents
			e.actOnWindowed(ctx, evaluated, acted, &heldCount, ramBound, tracker)
		}
	}
	if len(evaluated) > 0 {
		ramBound = heldCount >= e.maxTorrents
		e.actOnWindowed(ctx, evaluated, acted, &heldCount, ramBound, tracker)
	}

	e.logger.Info("scrape complete, updating what keep-at holds",
		"available", len(evaluated),
		"processed", processed,
		"total", totalCandidates,
		"elapsed", humanDuration(time.Since(scrapeStartedAt)),
		"library_size", humanBytes(stats.libraryBytes),
		"metadata_fetched", stats.metadataFetched.Load(),
		"metadata_cached", stats.metadataCached.Load(),
		"scrape_requests", stats.scrapeRequests.Load(),
		"scrape_cached", stats.scrapeCached.Load(),
		"skipped_held", stats.skippedHeld.Load(),
		"skipped_blocked", stats.skippedBlocked.Load(),
		"skipped_age", stats.skippedAge.Load(),
		"skipped_fetch_err", stats.skippedFetchErr.Load(),
		"skipped_scrape_err", stats.skippedScrapeErr.Load(),
		"eligible", stats.eligible.Load(),
	)

	e.saveNetworkStats(netstats.Snapshot{
		ScanStartedAt:       scanStartedAt,
		ScanCompletedAt:     time.Now().UTC(),
		TotalCandidates:     totalCandidates,
		ProcessedCandidates: processed,
		NodeCount:           tracker.NodeCount(),
		SeedingBytes:        tracker.SeedingBytes(),
		LeechingBytes:       tracker.LeechingBytes(),
	})

	// Keep the stats around so a smoke test (and the CLI, if it ever wants
	// to) can assert on what a scan actually did after the fact - e.g. that
	// scrapes were issued per candidate rather than dropped.
	e.lastScanStats = stats

	return nil
}

// actOnWindowed ranks the already-evaluated candidates and acts on the ones
// in the current top window that haven't been acted on yet. The window is the
// smaller of e.maxTorrents (the most torrents keep-at can hold) and the
// number evaluated so far, which guarantees we only ever seed candidates
// that are genuinely among the best - and that the best ones start seeding
// as soon as they're evaluated, without waiting for the full catalog.
func (e *Engine) actOnWindowed(ctx context.Context, evaluated []evaluatedCandidate, acted map[string]bool, heldCount *int, ramBound bool, tracker *netstats.Tracker) {
	ranked := rankEvaluated(evaluated, ramBound)
	// Only act on the top of the running ranking. The window is the smaller
	// of how many torrents keep-at can hold and how many candidates we've
	// evaluated so far - this guarantees we only ever seed candidates that
	// are genuinely among the best, and that the best ones start seeding as
	// soon as they're evaluated, without waiting for the full catalog.
	window := e.maxTorrents
	if len(ranked) < window {
		window = len(ranked)
	}
	for i := 0; i < window; i++ {
		c := ranked[i]
		key := c.infoHash.HexString()
		if acted[key] {
			continue
		}
		acted[key] = true
		safely(e.logger, "acting on "+c.title, func() {
			e.actOnCandidate(ctx, c, heldCount, tracker)
		})
	}
}

// countPendingCandidates counts catalog items ScanOnce will actually walk
// through this pass - excluding what's already held or keyword-blocked,
// both of which are free to check without any network calls - so progress
// reporting has a meaningful denominator before any fetching starts.
func countPendingCandidates(catalog atcatalog.Catalog, heldHashes map[string]bool, blocklist interface {
	Blocks(title, description string) (bool, string)
}) int {
	count := 0
	for _, item := range catalog.Items {
		if heldHashes[item.InfoHash.HexString()] {
			continue
		}
		if blocked, _ := blocklist.Blocks(item.Title, item.Description); blocked {
			continue
		}
		count++
	}
	return count
}

func (e *Engine) removeDeletedTorrents(held []state.Torrent, catalogHashes map[string]bool) {
	if e.cfg.PreserveDeletedTorrents {
		return
	}
	for _, h := range held {
		if catalogHashes[h.InfoHash.HexString()] {
			continue
		}
		e.logger.Info("removing torrent no longer listed on Academic Torrents", "title", h.Title)
		if err := e.RemoveTorrent(h.InfoHash, h.StorageLocation); err != nil {
			e.logger.Error("failed to remove deleted torrent", "title", h.Title, "err", err)
		}
	}
}

func (e *Engine) refreshHeldSeederCounts(ctx context.Context, held []state.Torrent, catalogHashes map[string]bool) {
	for _, h := range held {
		if !catalogHashes[h.InfoHash.HexString()] {
			continue // just removed above
		}
		h := h
		safely(e.logger, "refreshing seeder count for "+h.Title, func() {
			md, err := e.fetchMetadata(ctx, h.InfoHash, nil)
			if err != nil {
				e.logger.Warn("could not refresh metadata for held torrent", "title", h.Title, "err", err)
				return
			}
			swarm, err := e.scrapeSwarm(ctx, md.Trackers, h.InfoHash, nil)
			if err != nil {
				e.logger.Warn("could not scrape held torrent", "title", h.Title, "err", err)
				return
			}
			h.LastKnownSeeders = swarm.Seeders
			if err := e.state.Put(h); err != nil {
				e.logger.Error("failed to persist refreshed seeder count", "title", h.Title, "err", err)
			}
		})
	}
}

// evaluateCandidates walks the catalog and streams, over the returned
// channel, everything keep-at doesn't already hold, isn't keyword-blocked,
// and has aged past the moderation delay - each with a fresh metadata fetch
// (cached where possible) and tracker scrape. The swarm probe that feeds the
// anti-cascade decision is deliberately NOT done here; it's expensive
// (waits several seconds per candidate) and only needed at decision time, so
// ScanOnce probes on demand for just the candidates it's about to act on.
//
// Candidates are evaluated concurrently (see evaluateConcurrency) and emitted
// as they complete, so ScanOnce can start acting on the highest-priority ones
// before the whole catalog is evaluated. The channel is closed when the walk
// and all in-flight evaluations finish.
func (e *Engine) evaluateCandidates(ctx context.Context, catalog atcatalog.Catalog, heldHashes map[string]bool, tracker *netstats.Tracker, scanStartedAt time.Time, totalCandidates int, stats *scanStats) <-chan evaluatedCandidate {
	now := time.Now().UTC()
	minAge := e.cfg.Scan.ModerationDelay.AsDuration()

	var (
		processed atomic.Int64
		wg        sync.WaitGroup
	)
	sem := make(chan struct{}, evaluateConcurrency)
	results := make(chan evaluatedCandidate)

	// Production runs in its own goroutine so the channel can be returned
	// immediately: the caller (ScanOnce) starts draining results while
	// evaluation is still in flight, which is what makes acting incremental.
	// If we blocked on wg.Wait() here before returning, the caller could
	// never drain the channel (it hasn't received it yet) and evaluation
	// would deadlock waiting for the caller to consume.
	go func() {
		// currentSnapshot builds a fresh, self-contained snapshot from
		// thread-safe sources only (an atomic counter and tracker's own
		// mutex-guarded getters) - nothing here is a shared struct mutated by
		// multiple goroutines, which is what let the periodic save below and
		// the final save race safely.
		currentSnapshot := func() netstats.Snapshot {
			return netstats.Snapshot{
				ScanStartedAt:       scanStartedAt,
				TotalCandidates:     totalCandidates,
				ProcessedCandidates: int(processed.Load()),
				NodeCount:           tracker.NodeCount(),
				SeedingBytes:        tracker.SeedingBytes(),
				LeechingBytes:       tracker.LeechingBytes(),
			}
		}

		evalStartedAt := time.Now()

		stopProgress := make(chan struct{})
		progressStopped := make(chan struct{})
		go func() {
			defer close(progressStopped)
			saveTicker := time.NewTicker(progressSaveInterval)
			defer saveTicker.Stop()
			logTicker := time.NewTicker(progressLogInterval)
			defer logTicker.Stop()
			lastProbeReset := 0
			for {
				select {
				case <-stopProgress:
					return
				case <-saveTicker.C:
					e.saveNetworkStats(currentSnapshot())
					if p := int(processed.Load()); p-lastProbeReset >= probeClientResetInterval {
						lastProbeReset = p
						if err := e.resetProbeClient(); err != nil {
							e.logger.Warn("failed to reset probe client mid-scan", "err", err)
						}
					}
				case <-logTicker.C:
					e.logScrapeProgress(evalStartedAt, totalCandidates, int(processed.Load()))
				}
			}
		}()

	catalogLoop:
		for _, item := range catalog.Items {
			if ctx.Err() != nil {
				break catalogLoop
			}
			if heldHashes[item.InfoHash.HexString()] {
				stats.skippedHeld.Add(1)
				continue
			}
			if blocked, kw := e.blocklist.Blocks(item.Title, item.Description); blocked {
				stats.skippedBlocked.Add(1)
				e.logger.Debug("skipping blocked candidate", "title", item.Title, "keyword", kw)
				continue
			}

			select {
			case sem <- struct{}{}:
			case <-ctx.Done():
				break catalogLoop
			}

			item := item
			wg.Add(1)
			go func() {
				defer wg.Done()
				defer func() { <-sem }()
				defer processed.Add(1)

				safely(e.logger, "evaluating "+item.Title, func() {
					c, ok := e.evaluateOneCandidate(ctx, item, now, minAge, stats)
					if !ok {
						return
					}
					stats.eligible.Add(1)
					results <- c
				})
			}()
		}

		wg.Wait()
		close(stopProgress)
		<-progressStopped

		e.saveNetworkStats(currentSnapshot())
		close(results)
	}()

	return results
}

func (e *Engine) evaluateOneCandidate(ctx context.Context, item atcatalog.Item, now time.Time, minAge time.Duration, stats *scanStats) (evaluatedCandidate, bool) {
	md, err := e.fetchMetadata(ctx, item.InfoHash, stats)
	if err != nil {
		stats.skippedFetchErr.Add(1)
		e.logger.Warn("skipping candidate: could not fetch torrent metadata", "title", item.Title, "err", err)
		return evaluatedCandidate{}, false
	}

	if !filter.AgeEligible(md.CreatedAt, minAge, now) {
		stats.skippedAge.Add(1)
		return evaluatedCandidate{}, false
	}

	swarm, err := e.scrapeSwarm(ctx, md.Trackers, item.InfoHash, stats)
	if err != nil {
		stats.skippedScrapeErr.Add(1)
		e.logger.Warn("skipping candidate: could not scrape trackers", "title", item.Title, "err", err)
		return evaluatedCandidate{}, false
	}

	// No swarm probe here: probing waits several seconds per candidate and is
	// only needed at decision time, so tryAdd does it on demand for just the
	// candidates keep-at is about to act on.
	//
	// Only lightweight facts are carried onward (see evaluatedCandidate); the
	// full metadata is already cached to disk by fetchMetadata and will be
	// re-read from there if this candidate is acted on.
	return evaluatedCandidate{
		title:     item.Title,
		infoHash:  item.InfoHash,
		sizeBytes: md.Info.TotalLength(),
		swarm:     swarm,
	}, true
}

func (e *Engine) saveNetworkStats(snapshot netstats.Snapshot) {
	if err := netstats.Save(e.networkStatsPath(), snapshot); err != nil {
		e.logger.Warn("failed to persist network stats", "err", err)
	}
}

// logScrapeProgress writes a human-readable progress line - percent done,
// elapsed time, and an ETA extrapolated from the rate seen so far - while
// a scrape is still running. The ETA is a straight-line extrapolation
// (candidates processed so far / time elapsed), so it's only as good as
// the assumption that the rest of the catalog behaves like what's been
// seen already; it's meant to give a rough sense of progress, not a
// precise countdown.
func (e *Engine) logScrapeProgress(evalStartedAt time.Time, totalCandidates, processedCandidates int) {
	elapsed := time.Since(evalStartedAt)

	if totalCandidates <= 0 || processedCandidates <= 0 {
		e.logger.Info("scrape in progress", "processed", processedCandidates, "total", totalCandidates, "elapsed", humanDuration(elapsed))
		return
	}

	percent := float64(processedCandidates) / float64(totalCandidates) * 100
	remaining := totalCandidates - processedCandidates
	if remaining <= 0 {
		e.logger.Info("scrape in progress", "processed", processedCandidates, "total", totalCandidates, "elapsed", humanDuration(elapsed))
		return
	}

	perCandidate := elapsed / time.Duration(processedCandidates)
	eta := perCandidate * time.Duration(remaining)

	e.logger.Info("scrape in progress",
		"processed", processedCandidates,
		"total", totalCandidates,
		"percent", fmt.Sprintf("%.0f%%", percent),
		"elapsed", humanDuration(elapsed),
		"eta", humanDuration(eta))
}

// rankEvaluated converts evaluated candidates into selector.Candidate and
// orders them by seeding urgency. ramBound tells the selector to prefer
// larger torrents for the size tie-break when keep-at is already at its
// RAM-driven torrent cap (see selector.RankCandidates).
func rankEvaluated(candidates []evaluatedCandidate, ramBound bool) []evaluatedCandidate {
	sel := make([]selector.Candidate, len(candidates))
	byHash := make(map[string]evaluatedCandidate, len(candidates))
	for i, c := range candidates {
		sel[i] = selector.Candidate{
			InfoHash:  c.infoHash,
			Title:     c.title,
			SizeBytes: c.sizeBytes,
			Seeders:   c.swarm.Seeders,
			Leechers:  c.swarm.Leechers,
		}
		byHash[c.infoHash.HexString()] = c
	}

	ranked := selector.RankCandidates(sel, ramBound)
	out := make([]evaluatedCandidate, len(ranked))
	for i, r := range ranked {
		out[i] = byHash[r.InfoHash.HexString()]
	}
	return out
}

// actOnCandidate decides whether to seed candidate c, given the current held
// torrent count, and either fills free space or swaps out lower-priority held
// torrents. It's the per-candidate action step, called once per candidate in
// priority order (see ScanOnce's incremental loop). heldCount is the running
// count of held torrents so a successful add can bump it; a swap keeps the
// count constant and never bumps it. tracker records the keep-at peers found
// while probing this candidate's swarm, for network-status reporting.
//
// The RAM-driven torrent-count cap (e.maxTorrents) is enforced here: once
// keep-at is holding the maximum number of torrents its RAM budget allows, it
// stops adding new ones and only swaps (which keeps or lowers the count).
func (e *Engine) actOnCandidate(ctx context.Context, c evaluatedCandidate, heldCount *int, tracker *netstats.Tracker) {
	// Re-read the full metadata from keep-at's on-disk torrent cache now
	// that we're actually about to act on this candidate. It was fetched (and
	// cached) during evaluation but deliberately not kept in memory - the
	// piece-hash arrays are the biggest single memory cost of a scan, and
	// keeping them only for the handful of candidates acted on is what makes
	// keep-at's scan footprint work on small-RAM devices. This read hits the
	// torrent-cache directory, never the network.
	md, err := e.fetchMetadata(ctx, c.infoHash, nil)
	if err != nil {
		e.logger.Warn("could not reload cached metadata for candidate", "title", c.title, "err", err)
		return
	}

	sizeBytes := md.Info.TotalLength()

	// Probe this candidate's swarm on demand - only now, at decision time,
	// not during catalog evaluation. This is the anti-cascade signal
	// (how many other keep-at nodes are already here) and the network-status
	// peer data; it waits up to e.probeTimeout but only for torrents we're
	// actually about to act on, so it's a tiny fraction of the old cost.
	keepAtPeers := 0
	if c.swarm.Seeders+c.swarm.Leechers > 0 {
		observed, err := e.probeSwarm(ctx, md.MetaInfo, e.probeTimeout)
		if err != nil {
			e.logger.Warn("could not probe swarm", "title", c.title, "err", err)
		}
		for _, obs := range observed {
			tracker.Observe(obs.nodeKey, sizeBytes, obs.complete)
		}
		keepAtPeers = len(observed)
	}

	freeByPath := make(map[string]int64, len(e.cfg.Storage.Locations))
	for _, loc := range e.cfg.Storage.Locations {
		freeByPath[loc.Path] = e.freeBytes(loc)
	}

	// A plain free-space fill increases the torrent count, so it's only
	// allowed while under the RAM-driven cap. tryAdd also rejects it past
	// the cap as a backstop.
	if heldCount == nil || *heldCount < e.maxTorrents {
		if location, err := chooseLocation(e.cfg.Storage.Locations, freeByPath, sizeBytes, rand.Float64()); err == nil {
			if e.tryAdd(c, md, sizeBytes, location, nil, heldCount, keepAtPeers) {
				return
			}
		}
	} else {
		e.logger.Debug("skipping free-space fill: at RAM-driven torrent cap",
			"title", c.title, "held", *heldCount, "max_torrents", e.maxTorrents)
	}

	e.trySwap(c, md, sizeBytes, heldCount, keepAtPeers)
}

// tryAdd runs the anti-cascade decision and, if it passes, starts downloading
// the candidate into location. displaced is nil for a plain free-space fill.
// keepAtPeers is the count of other keep-at nodes observed in this candidate's
// swarm (gathered by the caller via probeSwarm), feeding the anti-cascade roll.
//
// heldCount points at the running count of held torrents so a successful add
// can bump it (a swap passes a non-nil displaced and keeps the count
// constant). A plain fill past the RAM-driven torrent cap is rejected
// outright: RAM scales per-torrent, so the count cap is the real memory bound.
func (e *Engine) tryAdd(c evaluatedCandidate, md *attorrent.Metadata, sizeBytes int64, location string, displaced []selector.Held, heldCount *int, keepAtPeers int) bool {
	if heldCount != nil && displaced == nil && *heldCount >= e.maxTorrents {
		e.logger.Debug("rejected candidate: would exceed RAM-driven torrent cap",
			"title", c.title, "held", *heldCount, "max_torrents", e.maxTorrents)
		return false
	}

	candidate := selector.Candidate{
		InfoHash:    c.infoHash,
		Title:       c.title,
		SizeBytes:   sizeBytes,
		Seeders:     c.swarm.Seeders,
		Leechers:    c.swarm.Leechers,
		KeepAtPeers: keepAtPeers,
	}

	decision := selector.EvaluateSwap(candidate, displaced, e.cfg.Scan.MinSeedMargin, e.cfg.Aggressiveness, rand.Float64())
	e.logger.Info("evaluated candidate", "title", c.title, "seeders", c.swarm.Seeders,
		"keep_at_peers", keepAtPeers, "should_swap", decision.ShouldSwap, "reason", decision.Reason)

	if !decision.ShouldSwap {
		return false
	}

	if err := e.AddCandidate(md, location, sizeBytes, c.title); err != nil {
		e.logger.Error("failed to add candidate", "title", c.title, "err", err)
		return false
	}
	if heldCount != nil && displaced == nil {
		*heldCount++
	}
	return true
}

// trySwap looks for held torrents, within a single storage location, that
// this candidate can justifiably displace - one is enough if it's big
// enough on its own, but if several smaller torrents each individually clear
// the seed margin against this candidate, and their combined size covers it,
// keep-at will remove all of them rather than only handling the single-torrent
// case.
//
// heldCount points at the running held count; a swap keeps the count constant
// (one in, >=1 out), so it's passed but never incremented. When keep-at is
// already at its RAM-driven torrent cap (ramBound), the eviction preference
// flips toward smaller held torrents so the scarce per-torrent RAM slots go to
// the largest torrents that fit - which is exactly the "prioritize larger
// torrents when RAM is the binding constraint" behavior. keepAtPeers is the
// on-demand swarm-probe count from the caller, fed into the swap decision.
func (e *Engine) trySwap(c evaluatedCandidate, md *attorrent.Metadata, sizeBytes int64, heldCount *int, keepAtPeers int) {
	ramBound := heldCount != nil && *heldCount >= e.maxTorrents
	held := e.state.All()

	byLocation := make(map[string][]state.Torrent)
	for _, h := range held {
		byLocation[h.StorageLocation] = append(byLocation[h.StorageLocation], h)
	}

	for location, inLocation := range byLocation {
		displaced := selectDisplaceable(inLocation, c.swarm.Seeders, sizeBytes, e.cfg.Scan.MinSeedMargin, ramBound)
		if displaced == nil {
			continue
		}

		selHeld := make([]selector.Held, len(displaced))
		for i, h := range displaced {
			selHeld[i] = selector.Held{InfoHash: h.InfoHash, Title: h.Title, SizeBytes: h.SizeBytes, Seeders: h.LastKnownSeeders}
		}

		if e.tryAdd(c, md, sizeBytes, location, selHeld, heldCount, keepAtPeers) {
			for _, h := range displaced {
				if err := e.RemoveTorrent(h.InfoHash, h.StorageLocation); err != nil {
					e.logger.Error("failed to remove displaced torrent", "title", h.Title, "err", err)
				}
			}
			return
		}
	}
}

// selectDisplaceable picks the smallest set of held torrents (within one
// location) that the candidate can displace: each one individually must
// clear the seed margin against the candidate (selector.MeetsSeedMargin
// checks the whole set against its minimum, so requiring every member to
// individually qualify - rather than relying on averaging - is what keeps
// this from evicting a torrent that wouldn't qualify on its own just
// because it's bundled with others that do), and their combined size must
// cover what the candidate needs. Returns nil if this location can't
// accommodate the swap even using every torrent that qualifies.
//
// ramBound indicates keep-at is already at its RAM-driven torrent cap. In
// that regime the binding constraint is the per-torrent RAM slot, not disk,
// so eviction flips from "most-seeded first" (least in need of keep-at) to
// "smallest first" - the scarce RAM slots go to the largest torrents that
// fit, the opposite of the disk-bound preference where we'd rather keep
// small torrents and shed big ones.
func selectDisplaceable(inLocation []state.Torrent, candidateSeeders int, sizeNeeded int64, minSeedMargin int, ramBound bool) []state.Torrent {
	var qualifying []state.Torrent
	for _, h := range inLocation {
		if h.LastKnownSeeders-minSeedMargin >= candidateSeeders {
			qualifying = append(qualifying, h)
		}
	}
	if len(qualifying) == 0 {
		return nil
	}

	if ramBound {
		// Free RAM slots with the least disk disruption: evict the smallest
		// qualifying torrents first so large held torrents (best bytes per
		// RAM slot) survive, and the candidate - which ranking has already
		// biased toward larger when RAM-bound - takes the freed slot.
		sort.SliceStable(qualifying, func(i, j int) bool { return qualifying[i].SizeBytes < qualifying[j].SizeBytes })
	} else {
		// Disk-bound: shed the least valuable (most-seeded, i.e. least in
		// need of keep-at specifically) torrents first.
		sort.SliceStable(qualifying, func(i, j int) bool { return qualifying[i].LastKnownSeeders > qualifying[j].LastKnownSeeders })
	}

	var chosen []state.Torrent
	var freed int64
	for _, h := range qualifying {
		chosen = append(chosen, h)
		freed += h.SizeBytes
		if freed >= sizeNeeded {
			return chosen
		}
	}
	return nil // even all qualifying torrents in this location aren't enough room
}
