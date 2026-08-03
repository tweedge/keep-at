package engine

import (
	"context"
	"math/rand"
	"sort"
	"time"

	"github.com/tweedge/mimisbaeti/internal/atcatalog"
	"github.com/tweedge/mimisbaeti/internal/attorrent"
	"github.com/tweedge/mimisbaeti/internal/filter"
	"github.com/tweedge/mimisbaeti/internal/selector"
	"github.com/tweedge/mimisbaeti/internal/state"
)

// evaluatedCandidate is a catalog item mimis has fetched metadata and a
// fresh scrape for, and is ready to rank and possibly act on.
type evaluatedCandidate struct {
	title    string
	metadata *attorrent.Metadata
	swarm    attorrent.SwarmCounts
}

// ScanOnce runs one full pass: refresh the catalog, drop anything Academic
// Torrents has taken down, refresh seed counts for what mimis already
// holds, and then look for new torrents to fill free space or displace
// lower-priority ones. It's expected to take a while on a large catalog -
// see PLAN.md - and is meant to be called periodically, not continuously.
func (e *Engine) ScanOnce(ctx context.Context) error {
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

	candidates := e.evaluateCandidates(ctx, catalog, heldHashes)
	ranked := rankEvaluated(candidates, e.cfg.Scan.MinSeedMargin)

	return e.actOnRanked(ctx, ranked)
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

// evaluateCandidates walks the catalog and returns everything mimis
// doesn't already hold, isn't keyword-blocked, and has aged past the
// moderation delay - each with a fresh metadata fetch (cached where
// possible) and tracker scrape.
func (e *Engine) evaluateCandidates(ctx context.Context, catalog atcatalog.Catalog, heldHashes map[string]bool) []evaluatedCandidate {
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

		md, err := e.fetchMetadata(ctx, item.InfoHash)
		if err != nil {
			e.logger.Warn("skipping candidate: could not fetch torrent metadata", "title", item.Title, "err", err)
			continue
		}

		if !filter.AgeEligible(md.CreatedAt, minAge, now) {
			continue
		}

		swarm, err := e.scrapeSwarm(ctx, md.Trackers, item.InfoHash)
		if err != nil {
			e.logger.Warn("skipping candidate: could not scrape trackers", "title", item.Title, "err", err)
			continue
		}

		out = append(out, evaluatedCandidate{title: item.Title, metadata: md, swarm: swarm})
	}
	return out
}

// rankEvaluated converts evaluated candidates into selector.Candidate and
// orders them by seeding urgency.
func rankEvaluated(candidates []evaluatedCandidate, _ int) []evaluatedCandidate {
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
// and falling back to displacing lower-priority held torrents (single for
// single; combining several smaller held torrents to fit one larger
// candidate isn't implemented in this version).
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
			if e.tryAdd(ctx, c, sizeBytes, location, nil) {
				continue
			}
		}

		e.trySwap(ctx, c, sizeBytes)
	}
	return nil
}

// tryAdd probes the candidate's swarm for other mimis nodes, runs the
// anti-cascade decision, and if it passes, starts downloading it into
// location. displaced is nil for a plain free-space fill.
func (e *Engine) tryAdd(ctx context.Context, c evaluatedCandidate, sizeBytes int64, location string, displaced []selector.Held) bool {
	mimisPeers, err := e.probeMimisPeerCount(ctx, c.metadata.MetaInfo, e.probeTimeout)
	if err != nil {
		e.logger.Warn("could not probe swarm for mimis peers", "title", c.title, "err", err)
	}

	candidate := selector.Candidate{
		InfoHash:   c.metadata.InfoHash,
		Title:      c.title,
		SizeBytes:  sizeBytes,
		Seeders:    c.swarm.Seeders,
		Leechers:   c.swarm.Leechers,
		MimisPeers: mimisPeers,
	}

	decision := selector.EvaluateSwap(candidate, displaced, e.cfg.Scan.MinSeedMargin, e.cfg.Aggressiveness, rand.Float64())
	e.logger.Info("evaluated candidate", "title", c.title, "seeders", c.swarm.Seeders,
		"mimis_peers", mimisPeers, "should_swap", decision.ShouldSwap, "reason", decision.Reason)

	if !decision.ShouldSwap {
		return false
	}

	if err := e.AddCandidate(c.metadata, location, sizeBytes, c.title); err != nil {
		e.logger.Error("failed to add candidate", "title", c.title, "err", err)
		return false
	}
	return true
}

// trySwap looks for a single held torrent, in a location with enough
// room once freed, that this candidate can justifiably displace.
func (e *Engine) trySwap(ctx context.Context, c evaluatedCandidate, sizeBytes int64) {
	held := e.state.All()
	sort.SliceStable(held, func(i, j int) bool { return held[i].LastKnownSeeders > held[j].LastKnownSeeders })

	for _, h := range held {
		if h.SizeBytes < sizeBytes {
			continue
		}
		displaced := []selector.Held{{InfoHash: h.InfoHash, Title: h.Title, SizeBytes: h.SizeBytes, Seeders: h.LastKnownSeeders}}
		if !selector.MeetsSeedMargin(c.swarm.Seeders, displaced, e.cfg.Scan.MinSeedMargin) {
			continue // cheap check first; skip the swarm probe entirely if this wouldn't qualify anyway
		}
		if e.tryAdd(ctx, c, sizeBytes, h.StorageLocation, displaced) {
			if err := e.RemoveTorrent(h.InfoHash, h.StorageLocation); err != nil {
				e.logger.Error("failed to remove displaced torrent", "title", h.Title, "err", err)
			}
			return
		}
	}
}
