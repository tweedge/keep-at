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
// probe (which counts other keep-at nodes for network-status) is
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

	// The seed-scarcity gate is anchored to the p10 seeder floor measured by
	// the most recent COMPLETED scan (see selector.SeederFloor). Load that
	// now so every candidate acted on during this scan is judged against the
	// catalog's known health. Zero - no completed scan yet, or nothing was
	// seeded - means the selector falls back to the conservative
	// aggressiveness^(seeders-1) behavior for this whole scan. It's carried
	// through every snapshot written during the scan, so a scan that dies
	// mid-way doesn't lose the anchor, and is replaced by the freshly
	// recomputed floor when this scan completes.
	seederFloor := 0
	if snap, err := netstats.Load(e.networkStatsPath()); err == nil {
		seederFloor = snap.SeederFloor
	}

	e.saveNetworkStats(netstats.Snapshot{ScanStartedAt: scanStartedAt, SeederFloor: seederFloor})

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
	e.evictStalledTorrents(catalogHashes)

	totalCandidates := countPendingCandidates(catalog, heldHashes, e.blocklist)
	e.saveNetworkStats(netstats.Snapshot{ScanStartedAt: scanStartedAt, TotalCandidates: totalCandidates, SeederFloor: seederFloor})
	e.logger.Info("starting scrape: fetching torrent metadata and tracker data for every pending catalog candidate to work out what needs seeding most - this can take a while on a large catalog, and downloads start gradually as the highest-priority candidates are found rather than waiting for the whole scrape to finish",
		"total", totalCandidates)

	stats := &scanStats{}
	for _, item := range catalog.Items {
		stats.libraryBytes += item.SizeBytes
	}

	tracker := netstats.NewTracker()
	scrapeStartedAt := time.Now()
	candChan := e.evaluateCandidates(ctx, catalog, heldHashes, tracker, scanStartedAt, totalCandidates, stats, seederFloor)

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
	//
	// The drain loop also watches ctx: the channel only closes after every
	// evaluation worker finishes (evaluateCandidates waits on them), so a
	// single stuck worker - a pathological torrent whose metadata parse or
	// tracker scrape never returns - would otherwise hold SIGTERM hostage
	// forever. Aborting the drain on cancellation is what lets `keep-at
	// stop` / `systemctl stop` shut down promptly even mid-scan.
	actEvery := evaluateConcurrency
drainLoop:
	for {
		select {
		case <-ctx.Done():
			break drainLoop
		case c, ok := <-candChan:
			if !ok {
				break drainLoop
			}
			processed++
			evaluated = append(evaluated, c)
			if processed%actEvery == 0 {
				ramBound = heldCount >= e.maxTorrents
				e.actOnWindowed(ctx, evaluated, acted, &heldCount, ramBound, tracker, seederFloor)
			}
		}
	}
	if ctx.Err() == nil && len(evaluated) > 0 {
		// Flush the last partial batch only when the scan wasn't
		// interrupted; acting on it while shutting down would just start
		// downloads we're about to abandon.
		ramBound = heldCount >= e.maxTorrents
		e.actOnWindowed(ctx, evaluated, acted, &heldCount, ramBound, tracker, seederFloor)
	}

	// A scan that was cut short - ctrl+c, a shutdown signal, or any other
	// cancellation - is not a completed scan. Marking it complete would set
	// ScanCompletedAt, which makes the next start wait out the remainder of
	// the scan interval before scanning again, so an aborted scan would push
	// the next real scan indefinitely far out. Instead, leave the snapshot
	// "in progress" (no ScanCompletedAt): the next start sees no completed
	// scan and scans immediately. The channel draining above is not evidence
	// of completion on its own - evaluateCandidates closes it after its
	// in-flight work finishes, which happens even when the catalog loop was
	// broken out of early by cancellation.
	if ctx.Err() != nil {
		return fmt.Errorf("engine: scan interrupted: %w", ctx.Err())
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
		"seeder_floor", selector.SeederFloor(evaluatedSeederCounts(evaluated)),
	)

	e.saveNetworkStats(netstats.Snapshot{
		ScanStartedAt:       scanStartedAt,
		ScanCompletedAt:     time.Now().UTC(),
		TotalCandidates:     totalCandidates,
		ProcessedCandidates: processed,
		NodeCount:           tracker.NodeCount(),
		SeedingBytes:        tracker.SeedingBytes(),
		LeechingBytes:       tracker.LeechingBytes(),
		SeederFloor:         selector.SeederFloor(evaluatedSeederCounts(evaluated)),
	})

	// Keep the stats around so a smoke test (and the CLI, if it ever wants
	// to) can assert on what a scan actually did after the fact - e.g. that
	// scrapes were issued per candidate rather than dropped.
	e.lastScanStats = stats

	return nil
}

// evaluatedSeederCounts collects the seeder count of every evaluated
// candidate, including zero-seeder ones (SeederFloor itself filters those
// out). It's the population the p10 seeder floor is computed over: the
// candidates keep-at actually considered this scan, which is the set whose
// health determines whether the catalog is improving.
func evaluatedSeederCounts(evaluated []evaluatedCandidate) []int {
	counts := make([]int, len(evaluated))
	for i, c := range evaluated {
		counts[i] = c.swarm.Seeders
	}
	return counts
}

// actOnWindowed ranks the already-evaluated candidates and acts on the ones
// in the current top window that haven't been acted on yet. The window is the
// smaller of e.maxTorrents (the most torrents keep-at can hold) and the
// number evaluated so far, which guarantees we only ever seed candidates
// that are genuinely among the best - and that the best ones start seeding
// as soon as they're evaluated, without waiting for the full catalog.
func (e *Engine) actOnWindowed(ctx context.Context, evaluated []evaluatedCandidate, acted map[string]bool, heldCount *int, ramBound bool, tracker *netstats.Tracker, seederFloor int) {
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
			e.actOnCandidate(ctx, c, heldCount, tracker, seederFloor)
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

			// Track download progress (completed pieces) so stalled
			// torrents - zero seeders, no new pieces - can be evicted. A
			// growing piece count means the torrent is alive even if the
			// scrape is zero (e.g. the tracker is unreachable but the swarm
			// is fine), so progress resets the stall clock.
			if store, ok := e.stores[h.StorageLocation]; ok {
				if pieces, err := store.CompletedPieceCount(h.InfoHash); err == nil {
					if pieces > h.CompletedPieces {
						h.CompletedPieces = pieces
						h.LastProgressAt = time.Now().UTC()
					} else if h.LastProgressAt.IsZero() {
						// First observation: seed the clock so the torrent
						// gets a full stall window before any eviction.
						h.CompletedPieces = pieces
						h.LastProgressAt = time.Now().UTC()
					}
				}
			}

			if err := e.state.Put(h); err != nil {
				e.logger.Error("failed to persist refreshed seeder count", "title", h.Title, "err", err)
			}
		})
	}
}

// evictStalledTorrents removes held torrents that have no live seeders and
// have made no download progress (no new completed pieces) since their stall
// clock started, for longer than the configured stall eviction timeout.
// Such a torrent can't complete - with zero seeders there's no one to serve
// the missing pieces - so it's occupying a RAM slot and disk accounting
// forever. Removing it frees both for a torrent that can actually download.
//
// The stall clock (state.Torrent.LastProgressAt) starts at first observation
// and resets whenever a torrent gains a piece, so a torrent only gets
// evicted after a full quiet period, not on its first zero-seeder scrape.
// Torrents still in the catalog are the only ones considered; ones that left
// the catalog are handled by removeDeletedTorrents. A stall eviction timeout
// of zero disables this entirely.
func (e *Engine) evictStalledTorrents(catalogHashes map[string]bool) {
	timeout := e.cfg.Scan.StallEvictionTimeout.AsDuration()
	if timeout <= 0 {
		return
	}

	now := time.Now().UTC()
	for _, h := range e.state.All() {
		if !catalogHashes[h.InfoHash.HexString()] {
			continue
		}
		if h.LastKnownSeeders > 0 {
			continue
		}
		if h.LastProgressAt.IsZero() {
			continue // no stall clock yet; refreshHeldSeederCounts starts one
		}
		if now.Sub(h.LastProgressAt) < timeout {
			continue
		}

		// No seeders and no new pieces for the whole timeout: give up.
		e.logger.Warn("removing stalled torrent: zero seeders and no download progress for the stall timeout",
			"title", h.Title,
			"stalled_for", humanDuration(now.Sub(h.LastProgressAt)),
			"timeout", humanDuration(timeout))
		if err := e.RemoveTorrent(h.InfoHash, h.StorageLocation); err != nil {
			e.logger.Error("failed to remove stalled torrent", "title", h.Title, "err", err)
		}
	}
}

// inflightCandidate is one candidate currently being evaluated: its title,
// when evaluation started, and (in a snapshot) how long it's been stuck.
type inflightCandidate struct {
	title     string
	startedAt time.Time
	elapsed   time.Duration
}

// inflightTracker tracks which candidates are currently being evaluated.
// Concurrent evaluation means up to evaluateConcurrency of them are in
// flight at once; the tracker exists so the periodic progress log can name
// them, which is how a pathological torrent (huge piece/file count, a
// tracker that never answers) gets identified instead of just being an
// unexplained slowdown.
type inflightTracker struct {
	mu   sync.Mutex
	byIH map[string]inflightCandidate
}

func newInflightTracker() *inflightTracker {
	return &inflightTracker{byIH: make(map[string]inflightCandidate)}
}

func (t *inflightTracker) start(infoHash metainfo.Hash, title string) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.byIH[infoHash.HexString()] = inflightCandidate{title: title, startedAt: time.Now()}
}

func (t *inflightTracker) end(infoHash metainfo.Hash) {
	t.mu.Lock()
	defer t.mu.Unlock()
	delete(t.byIH, infoHash.HexString())
}

// snapshot returns the in-flight candidates with their elapsed durations,
// longest first, so the longest-stalled one is the first thing an operator
// sees in the progress log.
func (t *inflightTracker) snapshot() []inflightCandidate {
	t.mu.Lock()
	defer t.mu.Unlock()
	out := make([]inflightCandidate, 0, len(t.byIH))
	now := time.Now()
	for _, c := range t.byIH {
		c.elapsed = now.Sub(c.startedAt)
		out = append(out, c)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].elapsed > out[j].elapsed })
	return out
}

// evaluateCandidates walks the catalog and streams, over the returned
// channel, everything keep-at doesn't already hold, isn't keyword-blocked,
// and has aged past the moderation delay - each with a fresh metadata fetch
// (cached where possible) and tracker scrape. The swarm probe that counts
// other keep-at nodes for network-status is deliberately NOT done here; it's
// expensive (waits several seconds per candidate) and only needed at
// decision time, so ScanOnce probes on demand for just the candidates it's
// about to act on.
//
// Candidates are evaluated concurrently (see evaluateConcurrency) and emitted
// as they complete, so ScanOnce can start acting on the highest-priority ones
// before the whole catalog is evaluated. The channel is closed when the walk
// and all in-flight evaluations finish.
func (e *Engine) evaluateCandidates(ctx context.Context, catalog atcatalog.Catalog, heldHashes map[string]bool, tracker *netstats.Tracker, scanStartedAt time.Time, totalCandidates int, stats *scanStats, seederFloor int) <-chan evaluatedCandidate {
	now := time.Now().UTC()
	minAge := e.cfg.Scan.ModerationDelay.AsDuration()

	var (
		processed atomic.Int64
		wg        sync.WaitGroup
	)
	sem := make(chan struct{}, evaluateConcurrency)
	results := make(chan evaluatedCandidate)
	inflight := newInflightTracker()

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
				SeederFloor:         seederFloor,
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
					e.logScrapeProgress(evalStartedAt, totalCandidates, int(processed.Load()), inflight)
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
				inflight.start(item.InfoHash, item.Title)
				defer inflight.end(item.InfoHash)

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
//
// inflight names the candidates currently being evaluated, longest-stalled
// first. A torrent that takes far longer than its peers to fetch metadata
// or scrape trackers shows up here, which is what lets an operator identify
// a pathological torrent instead of just seeing an unexplained slowdown.
func (e *Engine) logScrapeProgress(evalStartedAt time.Time, totalCandidates, processedCandidates int, inflight *inflightTracker) {
	elapsed := time.Since(evalStartedAt)

	// Name every in-flight candidate. Titles can contain characters that
	// don't play nicely as log keys, so they're carried as a single value.
	var stuck []string
	for _, c := range inflight.snapshot() {
		stuck = append(stuck, fmt.Sprintf("%q (%s)", c.title, humanDuration(c.elapsed)))
	}

	if totalCandidates <= 0 || processedCandidates <= 0 {
		e.logger.Info("scrape in progress", "processed", processedCandidates, "total", totalCandidates, "elapsed", humanDuration(elapsed), "currently_scraping", stuck)
		return
	}

	percent := float64(processedCandidates) / float64(totalCandidates) * 100
	remaining := totalCandidates - processedCandidates
	if remaining <= 0 {
		e.logger.Info("scrape in progress", "processed", processedCandidates, "total", totalCandidates, "elapsed", humanDuration(elapsed), "currently_scraping", stuck)
		return
	}

	perCandidate := elapsed / time.Duration(processedCandidates)
	eta := perCandidate * time.Duration(remaining)

	e.logger.Info("scrape in progress",
		"processed", processedCandidates,
		"total", totalCandidates,
		"percent", fmt.Sprintf("%.0f%%", percent),
		"elapsed", humanDuration(elapsed),
		"eta", humanDuration(eta),
		"currently_scraping", stuck)
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
func (e *Engine) actOnCandidate(ctx context.Context, c evaluatedCandidate, heldCount *int, tracker *netstats.Tracker, seederFloor int) {
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
	// not during catalog evaluation. This counts other keep-at nodes for
	// network-status and peer data; it waits up to e.probeTimeout but only
	// for torrents we're actually about to act on, so it's a tiny fraction
	// of the old cost. The keep-at peer count is metadata only - selection
	// is gated on total seeders, not on it (see selector.SelectionChance).
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
	sizeByPath := make(map[string]int64, len(e.cfg.Storage.Locations))
	for _, loc := range e.cfg.Storage.Locations {
		freeByPath[loc.Path] = e.freeBytes(loc)
		sizeByPath[loc.Path] = e.estimatedOnDiskBytes(loc.Path, sizeBytes)
	}

	// A plain free-space fill increases the torrent count, so it's only
	// allowed while under the RAM-driven cap. tryAdd also rejects it past
	// the cap as a backstop. The decision from whichever attempt runs last
	// (fill, then swap) is logged once, below - so each candidate produces
	// exactly one "evaluated candidate" line.
	decision := selector.SwapDecision{ShouldSwap: false, Reason: "candidate not evaluated"}
	added := false
	if heldCount == nil || *heldCount < e.maxTorrents {
		if location, err := chooseLocation(e.cfg.Storage.Locations, freeByPath, sizeByPath, rand.Float64()); err == nil {
			var d selector.SwapDecision
			added, d = e.tryAdd(c, md, sizeBytes, location, nil, heldCount, keepAtPeers, seederFloor)
			decision = d
			if added {
				e.logEvaluatedCandidate(c, decision, keepAtPeers)
				return
			}
		}
	} else {
		e.logger.Debug("skipping free-space fill: at RAM-driven torrent cap",
			"title", c.title, "held", *heldCount, "max_torrents", e.maxTorrents)
	}

	if swapped, d := e.trySwap(c, md, sizeBytes, heldCount, keepAtPeers, seederFloor); swapped {
		decision = d
	} else if !decision.SeedScarcityBlocked() && d.Reason != "" {
		// trySwap failed, but it returns the most informative decision it
		// made (e.g. a seed-scarcity roll failure when the fill path was
		// skipped at the RAM cap). Adopt it unless the fill path already
		// settled the outcome with its own seed-scarcity rejection - that
		// rejection is the final answer for the candidate, and it's
		// deliberately not logged (see logEvaluatedCandidate).
		decision = d
	}
	e.logEvaluatedCandidate(c, decision, keepAtPeers)
}

// logEvaluatedCandidate emits the single "evaluated candidate" line for a
// candidate after its decision is final. This is deliberately one log line
// per candidate, even though the fill and swap paths each run their own
// selection roll: the operator wants to see what happened to the candidate,
// not duplicate noise from the internal fill-then-swap attempt sequence.
//
// A candidate rejected by the seed-scarcity roll is deliberately NOT logged:
// that's the routine outcome for any well-seeded candidate (the expected
// case, not an error), and at catalog scale there are hundreds of them per
// scan, so logging each one drowns out the lines that actually matter. See
// selector.ReasonSeedScarcityRollFailed. Every other outcome - added,
// swapped, margin failure, RAM cap, etc. - is still logged.
func (e *Engine) logEvaluatedCandidate(c evaluatedCandidate, decision selector.SwapDecision, keepAtPeers int) {
	if decision.SeedScarcityBlocked() {
		return
	}
	e.logger.Info("evaluated candidate",
		"title", c.title,
		"seeders", c.swarm.Seeders,
		"keep_at_peers", keepAtPeers,
		"should_swap", decision.ShouldSwap,
		"reason", decision.Reason)
}

// tryAdd runs the selection decision and, if it passes, starts downloading
// the candidate into location. displaced is nil for a plain free-space fill.
// keepAtPeers is the count of other keep-at nodes observed in this
// candidate's swarm (gathered by the caller via probeSwarm); it's recorded
// for network-status and logged as metadata, but the selection gate itself
// is keyed on total seeders - keep-at seeds minimally-seeded torrents, not
// everything it sees.
//
// heldCount points at the running count of held torrents so a successful add
// can bump it (a swap passes a non-nil displaced and keeps the count
// constant). A plain fill past the RAM-driven torrent cap is rejected
// outright: RAM scales per-torrent, so the count cap is the real memory bound.
func (e *Engine) tryAdd(c evaluatedCandidate, md *attorrent.Metadata, sizeBytes int64, location string, displaced []selector.Held, heldCount *int, keepAtPeers int, seederFloor int) (bool, selector.SwapDecision) {
	if heldCount != nil && displaced == nil && *heldCount >= e.maxTorrents {
		e.logger.Debug("rejected candidate: would exceed RAM-driven torrent cap",
			"title", c.title, "held", *heldCount, "max_torrents", e.maxTorrents)
		return false, selector.SwapDecision{ShouldSwap: false, Reason: "RAM-driven torrent cap reached"}
	}

	candidate := selector.Candidate{
		InfoHash:    c.infoHash,
		Title:       c.title,
		SizeBytes:   sizeBytes,
		Seeders:     c.swarm.Seeders,
		Leechers:    c.swarm.Leechers,
		KeepAtPeers: keepAtPeers,
		SeederFloor: seederFloor,
	}

	decision := selector.EvaluateSwap(candidate, displaced, e.cfg.Scan.MinSeedMargin, e.cfg.Aggressiveness, rand.Float64())

	if !decision.ShouldSwap {
		return false, decision
	}

	if err := e.AddCandidate(md, location, sizeBytes, c.title); err != nil {
		e.logger.Error("failed to add candidate", "title", c.title, "err", err)
		return false, decision
	}
	if heldCount != nil && displaced == nil {
		*heldCount++
	}
	return true, decision
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
// It returns whether a swap happened, and the swap decision that was made.
func (e *Engine) trySwap(c evaluatedCandidate, md *attorrent.Metadata, sizeBytes int64, heldCount *int, keepAtPeers int, seederFloor int) (bool, selector.SwapDecision) {
	ramBound := heldCount != nil && *heldCount >= e.maxTorrents
	held := e.state.All()

	byLocation := make(map[string][]state.Torrent)
	for _, h := range held {
		byLocation[h.StorageLocation] = append(byLocation[h.StorageLocation], h)
	}

	// The last decision made is what gets reported if no location ends up
	// accommodating the swap, so actOnCandidate's single "evaluated
	// candidate" line shows the real reason (including a seed-scarcity roll
	// failure, which the engine suppresses) rather than a generic empty one.
	lastDecision := selector.SwapDecision{ShouldSwap: false, Reason: "no held torrent clears the seed margin against this candidate"}
	for location, inLocation := range byLocation {
		// Both the candidate's needed space and the freed space from
		// displacing held torrents are measured post-compression (actual
		// on-disk bytes / estimated on-disk footprint), so compression gains
		// count toward fitting more torrents here just as they do for
		// free-space fills.
		sizeNeeded := e.estimatedOnDiskBytes(location, sizeBytes)
		displaced := selectDisplaceable(inLocation, c.swarm.Seeders, sizeNeeded, e.cfg.Scan.MinSeedMargin, ramBound, e.heldOnDiskBytes)
		if displaced == nil {
			continue
		}

		selHeld := make([]selector.Held, len(displaced))
		for i, h := range displaced {
			selHeld[i] = selector.Held{InfoHash: h.InfoHash, Title: h.Title, SizeBytes: h.SizeBytes, Seeders: h.LastKnownSeeders}
		}

		if ok, decision := e.tryAdd(c, md, sizeBytes, location, selHeld, heldCount, keepAtPeers, seederFloor); ok {
			for _, h := range displaced {
				if err := e.RemoveTorrent(h.InfoHash, h.StorageLocation); err != nil {
					e.logger.Error("failed to remove displaced torrent", "title", h.Title, "err", err)
				}
			}
			return true, decision
		} else {
			lastDecision = decision
		}
	}
	return false, lastDecision
}

// heldOnDiskBytes returns a held torrent's actual on-disk (post-compression)
// footprint, falling back to its nominal size if the store can't be queried.
// Swap math uses this so a displaced torrent's real freed space is what
// counts against the candidate's estimated footprint.
func (e *Engine) heldOnDiskBytes(h state.Torrent) int64 {
	if store, ok := e.stores[h.StorageLocation]; ok {
		if n, err := store.DiskUsage(h.InfoHash); err == nil {
			return n
		}
	}
	return h.SizeBytes
}

// selectDisplaceable picks the smallest set of held torrents (within one
// location) that the candidate can displace: each one individually must
// clear the seed margin against the candidate (selector.MeetsSeedMargin
// checks the whole set against its minimum, so requiring every member to
// individually qualify - rather than relying on averaging - is what keeps
// this from evicting a torrent that wouldn't qualify on its own just
// because it's bundled with others that do), and their combined freed size
// must cover what the candidate needs. Returns nil if this location can't
// accommodate the swap even using every torrent that qualifies.
//
// sizeOf resolves each held torrent's freed size (post-compression on-disk
// bytes in production, nominal size in tests). sizeNeeded is the candidate's
// estimated on-disk footprint; both being measured the same way is what keeps
// compression gains from leaking out of the swap math.
//
// ramBound indicates keep-at is already at its RAM-driven torrent cap. In
// that regime the binding constraint is the per-torrent RAM slot, not disk,
// so eviction flips from "most-seeded first" (least in need of keep-at) to
// "smallest first" - the scarce RAM slots go to the largest torrents that
// fit, the opposite of the disk-bound preference where we'd rather keep
// small torrents and shed big ones.
func selectDisplaceable(inLocation []state.Torrent, candidateSeeders int, sizeNeeded int64, minSeedMargin int, ramBound bool, sizeOf func(state.Torrent) int64) []state.Torrent {
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
		freed += sizeOf(h)
		if freed >= sizeNeeded {
			return chosen
		}
	}
	return nil // even all qualifying torrents in this location aren't enough room
}
