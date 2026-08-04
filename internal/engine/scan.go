package engine

import (
	"context"
	"fmt"
	"math/rand"
	"sort"
	"sync"
	"sync/atomic"
	"time"

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
const progressLogInterval = 2 * time.Minute

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
// fresh scrape (and, if anyone was around to scrape, a swarm probe) for,
// and is ready to rank and possibly act on.
type evaluatedCandidate struct {
	title       string
	metadata    *attorrent.Metadata
	swarm       attorrent.SwarmCounts
	keepAtPeers int // from probing the swarm once during evaluateCandidates; reused by tryAdd
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
	e.logger.Info("starting scrape: fetching torrent metadata and tracker data for every pending catalog candidate to work out what needs seeding most - this determines priority before anything is downloaded, so it can take a while on a large catalog, and keep-at won't start changing what it holds until it finishes",
		"total", totalCandidates)

	tracker := netstats.NewTracker()
	scrapeStartedAt := time.Now()
	candidates, processedCount := e.evaluateCandidates(ctx, catalog, heldHashes, tracker, scanStartedAt, totalCandidates)
	e.logger.Info("scrape complete, updating what keep-at holds",
		"available", len(candidates), "processed", processedCount, "total", totalCandidates, "elapsed", humanDuration(time.Since(scrapeStartedAt)))

	ranked := rankEvaluated(candidates)
	actErr := e.actOnRanked(ctx, ranked)

	e.saveNetworkStats(netstats.Snapshot{
		ScanStartedAt:       scanStartedAt,
		ScanCompletedAt:     time.Now().UTC(),
		TotalCandidates:     totalCandidates,
		ProcessedCandidates: processedCount,
		NodeCount:           tracker.NodeCount(),
		SeedingBytes:        tracker.SeedingBytes(),
		LeechingBytes:       tracker.LeechingBytes(),
	})

	return actErr
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
			md, err := e.fetchMetadata(ctx, h.InfoHash)
			if err != nil {
				e.logger.Warn("could not refresh metadata for held torrent", "title", h.Title, "err", err)
				return
			}
			swarm, err := e.scrapeSwarm(ctx, md.Trackers, h.InfoHash)
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

// evaluateCandidates walks the catalog and returns everything keep-at
// doesn't already hold, isn't keyword-blocked, and has aged past the
// moderation delay - each with a fresh metadata fetch (cached where
// possible) and tracker scrape. Every candidate anyone at all is seeding
// or leeching also gets a quick swarm probe, which feeds both the
// anti-cascade decision later and the network-wide stats in tracker.
//
// Candidates are evaluated concurrently (see evaluateConcurrency); order
// doesn't matter here since rankEvaluated sorts the result afterward.
func (e *Engine) evaluateCandidates(ctx context.Context, catalog atcatalog.Catalog, heldHashes map[string]bool, tracker *netstats.Tracker, scanStartedAt time.Time, totalCandidates int) ([]evaluatedCandidate, int) {
	now := time.Now().UTC()
	minAge := e.cfg.Scan.ModerationDelay.AsDuration()

	var (
		mu        sync.Mutex
		out       []evaluatedCandidate
		processed atomic.Int64
		wg        sync.WaitGroup
	)
	sem := make(chan struct{}, evaluateConcurrency)

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
			continue
		}
		if blocked, kw := e.blocklist.Blocks(item.Title, item.Description); blocked {
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
				c, ok := e.evaluateOneCandidate(ctx, item, now, minAge, tracker)
				if !ok {
					return
				}
				mu.Lock()
				out = append(out, c)
				mu.Unlock()
			})
		}()
	}

	wg.Wait()
	close(stopProgress)
	<-progressStopped

	e.saveNetworkStats(currentSnapshot())
	return out, int(processed.Load())
}

func (e *Engine) evaluateOneCandidate(ctx context.Context, item atcatalog.Item, now time.Time, minAge time.Duration, tracker *netstats.Tracker) (evaluatedCandidate, bool) {
	md, err := e.fetchMetadata(ctx, item.InfoHash)
	if err != nil {
		e.logger.Warn("skipping candidate: could not fetch torrent metadata", "title", item.Title, "err", err)
		return evaluatedCandidate{}, false
	}

	if !filter.AgeEligible(md.CreatedAt, minAge, now) {
		return evaluatedCandidate{}, false
	}

	swarm, err := e.scrapeSwarm(ctx, md.Trackers, item.InfoHash)
	if err != nil {
		e.logger.Warn("skipping candidate: could not scrape trackers", "title", item.Title, "err", err)
		return evaluatedCandidate{}, false
	}

	keepAtPeerCount := 0
	if swarm.Seeders+swarm.Leechers > 0 {
		observed, err := e.probeSwarm(ctx, md.MetaInfo, e.probeTimeout)
		if err != nil {
			e.logger.Warn("could not probe swarm", "title", item.Title, "err", err)
		}
		sizeBytes := md.Info.TotalLength()
		for _, obs := range observed {
			tracker.Observe(obs.nodeKey, sizeBytes, obs.complete)
		}
		keepAtPeerCount = len(observed)
	}

	return evaluatedCandidate{title: item.Title, metadata: md, swarm: swarm, keepAtPeers: keepAtPeerCount}, true
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
// orders them by seeding urgency.
func rankEvaluated(candidates []evaluatedCandidate) []evaluatedCandidate {
	sel := make([]selector.Candidate, len(candidates))
	byHash := make(map[string]evaluatedCandidate, len(candidates))
	for i, c := range candidates {
		sel[i] = selector.Candidate{
			InfoHash:  c.metadata.InfoHash,
			Title:     c.title,
			SizeBytes: c.metadata.Info.TotalLength(),
			Seeders:   c.swarm.Seeders,
			Leechers:  c.swarm.Leechers,
		}
		byHash[c.metadata.InfoHash.HexString()] = c
	}

	ranked := selector.RankCandidates(sel)
	out := make([]evaluatedCandidate, len(ranked))
	for i, r := range ranked {
		out[i] = byHash[r.InfoHash.HexString()]
	}
	return out
}

// actOnRanked walks candidates in priority order, filling free space first
// and falling back to displacing lower-priority held torrents.
func (e *Engine) actOnRanked(ctx context.Context, ranked []evaluatedCandidate) error {
	for _, c := range ranked {
		if ctx.Err() != nil {
			return ctx.Err()
		}

		c := c
		safely(e.logger, "acting on "+c.title, func() {
			sizeBytes := c.metadata.Info.TotalLength()

			freeByPath := make(map[string]int64, len(e.cfg.Storage.Locations))
			for _, loc := range e.cfg.Storage.Locations {
				freeByPath[loc.Path] = e.freeBytes(loc)
			}

			if location, err := chooseLocation(e.cfg.Storage.Locations, freeByPath, sizeBytes, rand.Float64()); err == nil {
				if e.tryAdd(c, sizeBytes, location, nil) {
					return
				}
			}

			e.trySwap(c, sizeBytes)
		})
	}
	return nil
}

// tryAdd runs the anti-cascade decision using the swarm probe already
// gathered in evaluateCandidates, and if it passes, starts downloading the
// candidate into location. displaced is nil for a plain free-space fill.
func (e *Engine) tryAdd(c evaluatedCandidate, sizeBytes int64, location string, displaced []selector.Held) bool {
	candidate := selector.Candidate{
		InfoHash:    c.metadata.InfoHash,
		Title:       c.title,
		SizeBytes:   sizeBytes,
		Seeders:     c.swarm.Seeders,
		Leechers:    c.swarm.Leechers,
		KeepAtPeers: c.keepAtPeers,
	}

	decision := selector.EvaluateSwap(candidate, displaced, e.cfg.Scan.MinSeedMargin, e.cfg.Aggressiveness, rand.Float64())
	e.logger.Info("evaluated candidate", "title", c.title, "seeders", c.swarm.Seeders,
		"keep_at_peers", c.keepAtPeers, "should_swap", decision.ShouldSwap, "reason", decision.Reason)

	if !decision.ShouldSwap {
		return false
	}

	if err := e.AddCandidate(c.metadata, location, sizeBytes, c.title); err != nil {
		e.logger.Error("failed to add candidate", "title", c.title, "err", err)
		return false
	}
	return true
}

// trySwap looks for held torrents, within a single storage location, that
// this candidate can justifiably displace - one is enough if it's big
// enough on its own, but if several smaller torrents each individually
// clear the seed margin against this candidate, and their combined size
// covers it, keep-at will remove all of them rather than only handling the
// single-torrent case.
func (e *Engine) trySwap(c evaluatedCandidate, sizeBytes int64) {
	held := e.state.All()

	byLocation := make(map[string][]state.Torrent)
	for _, h := range held {
		byLocation[h.StorageLocation] = append(byLocation[h.StorageLocation], h)
	}

	for location, inLocation := range byLocation {
		displaced := selectDisplaceable(inLocation, c.swarm.Seeders, sizeBytes, e.cfg.Scan.MinSeedMargin)
		if displaced == nil {
			continue
		}

		selHeld := make([]selector.Held, len(displaced))
		for i, h := range displaced {
			selHeld[i] = selector.Held{InfoHash: h.InfoHash, Title: h.Title, SizeBytes: h.SizeBytes, Seeders: h.LastKnownSeeders}
		}

		if e.tryAdd(c, sizeBytes, location, selHeld) {
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
func selectDisplaceable(inLocation []state.Torrent, candidateSeeders int, sizeNeeded int64, minSeedMargin int) []state.Torrent {
	var qualifying []state.Torrent
	for _, h := range inLocation {
		if h.LastKnownSeeders-minSeedMargin >= candidateSeeders {
			qualifying = append(qualifying, h)
		}
	}
	if len(qualifying) == 0 {
		return nil
	}

	// Displace the least valuable (most-seeded, i.e. least in need of
	// keep-at specifically) torrents first.
	sort.SliceStable(qualifying, func(i, j int) bool { return qualifying[i].LastKnownSeeders > qualifying[j].LastKnownSeeders })

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
