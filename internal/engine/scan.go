package engine

import (
	"context"
	"math/rand"
	"sort"
	"time"

	"github.com/tweedge/keep-at/internal/atcatalog"
	"github.com/tweedge/keep-at/internal/attorrent"
	"github.com/tweedge/keep-at/internal/filter"
	"github.com/tweedge/keep-at/internal/netstats"
	"github.com/tweedge/keep-at/internal/selector"
	"github.com/tweedge/keep-at/internal/state"
)

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
	snapshot := netstats.Snapshot{ScanStartedAt: time.Now().UTC()}
	e.saveNetworkStats(snapshot)

	catalog, err := e.catalogFetcher.Load(ctx, e.cfg.Scan.Interval.AsDuration())
	if err != nil && len(catalog.Items) == 0 {
		return err
	}
	if err != nil {
		e.logger.Warn("catalog refresh failed, continuing with stale cache", "err", err)
	}

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

	snapshot.TotalCandidates = countPendingCandidates(catalog, heldHashes, e.blocklist)
	e.saveNetworkStats(snapshot)

	tracker := netstats.NewTracker()
	candidates := e.evaluateCandidates(ctx, catalog, heldHashes, tracker, &snapshot)

	ranked := rankEvaluated(candidates)
	actErr := e.actOnRanked(ctx, ranked)

	snapshot.ScanCompletedAt = time.Now().UTC()
	snapshot.NodeCount = tracker.NodeCount()
	snapshot.SeedingBytes = tracker.SeedingBytes()
	snapshot.LeechingBytes = tracker.LeechingBytes()
	e.saveNetworkStats(snapshot)

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
		md, err := e.fetchMetadata(ctx, h.InfoHash)
		if err != nil {
			e.logger.Warn("could not refresh metadata for held torrent", "title", h.Title, "err", err)
			continue
		}
		swarm, err := e.scrapeSwarm(ctx, md.Trackers, h.InfoHash)
		if err != nil {
			e.logger.Warn("could not scrape held torrent", "title", h.Title, "err", err)
			continue
		}
		h.LastKnownSeeders = swarm.Seeders
		if err := e.state.Put(h); err != nil {
			e.logger.Error("failed to persist refreshed seeder count", "title", h.Title, "err", err)
		}
	}
}

// evaluateCandidates walks the catalog and returns everything keep-at
// doesn't already hold, isn't keyword-blocked, and has aged past the
// moderation delay - each with a fresh metadata fetch (cached where
// possible) and tracker scrape. Every candidate anyone at all is seeding
// or leeching also gets a quick swarm probe, which feeds both the
// anti-cascade decision later and the network-wide stats in tracker.
func (e *Engine) evaluateCandidates(ctx context.Context, catalog atcatalog.Catalog, heldHashes map[string]bool, tracker *netstats.Tracker, snapshot *netstats.Snapshot) []evaluatedCandidate {
	now := time.Now().UTC()
	minAge := e.cfg.Scan.ModerationDelay.AsDuration()

	var out []evaluatedCandidate
	for _, item := range catalog.Items {
		if ctx.Err() != nil {
			return out
		}
		if heldHashes[item.InfoHash.HexString()] {
			continue
		}
		if blocked, kw := e.blocklist.Blocks(item.Title, item.Description); blocked {
			e.logger.Debug("skipping blocked candidate", "title", item.Title, "keyword", kw)
			continue
		}

		out = e.evaluateOneCandidate(ctx, item, now, minAge, tracker, out)

		snapshot.ProcessedCandidates++
		snapshot.NodeCount = tracker.NodeCount()
		snapshot.SeedingBytes = tracker.SeedingBytes()
		snapshot.LeechingBytes = tracker.LeechingBytes()
		e.saveNetworkStats(*snapshot)
	}
	return out
}

func (e *Engine) evaluateOneCandidate(ctx context.Context, item atcatalog.Item, now time.Time, minAge time.Duration, tracker *netstats.Tracker, out []evaluatedCandidate) []evaluatedCandidate {
	md, err := e.fetchMetadata(ctx, item.InfoHash)
	if err != nil {
		e.logger.Warn("skipping candidate: could not fetch torrent metadata", "title", item.Title, "err", err)
		return out
	}

	if !filter.AgeEligible(md.CreatedAt, minAge, now) {
		return out
	}

	swarm, err := e.scrapeSwarm(ctx, md.Trackers, item.InfoHash)
	if err != nil {
		e.logger.Warn("skipping candidate: could not scrape trackers", "title", item.Title, "err", err)
		return out
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

	return append(out, evaluatedCandidate{title: item.Title, metadata: md, swarm: swarm, keepAtPeers: keepAtPeerCount})
}

func (e *Engine) saveNetworkStats(snapshot netstats.Snapshot) {
	if err := netstats.Save(e.networkStatsPath(), snapshot); err != nil {
		e.logger.Warn("failed to persist network stats", "err", err)
	}
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

		sizeBytes := c.metadata.Info.TotalLength()

		freeByPath := make(map[string]int64, len(e.cfg.Storage.Locations))
		for _, loc := range e.cfg.Storage.Locations {
			freeByPath[loc.Path] = e.freeBytes(loc)
		}

		if location, err := chooseLocation(e.cfg.Storage.Locations, freeByPath, sizeBytes, rand.Float64()); err == nil {
			if e.tryAdd(c, sizeBytes, location, nil) {
				continue
			}
		}

		e.trySwap(c, sizeBytes)
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
