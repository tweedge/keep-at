// Package selector implements keep-at's two central decisions: which torrent
// is most urgently in need of seeding, and whether swapping to it right now
// would risk a cascade (every keep-at node globally piling onto the same
// under-seeded torrent at once).
package selector

import (
	"math"
	"sort"

	"github.com/anacrolix/torrent/metainfo"
)

// Candidate is a torrent keep-at is considering downloading, combining
// catalog metadata with a fresh tracker scrape.
type Candidate struct {
	InfoHash    metainfo.Hash
	Title       string
	SizeBytes   int64
	Seeders     int
	Leechers    int
	KeepAtPeers int // peers in the swarm that self-identified as keep-at, see buildinfo.ClientName
}

// Available reports whether a candidate meets the availability bar: at
// least one live seed. The plan also allows "peers with sufficient data"
// (i.e. leechers whose combined pieces cover 100% of the torrent even with
// zero full seeds) to count as available, but verifying that requires
// per-peer piece-map inspection across the whole swarm, which is expensive
// to do for every catalog candidate during a scan. keep-at uses the cheaper,
// conservative signal instead: a torrent isn't a candidate until someone
// can serve 100% of it outright.
func (c Candidate) Available() bool {
	return c.Seeders >= 1
}

// Held is a torrent keep-at currently stores and seeds.
type Held struct {
	InfoHash  metainfo.Hash
	Title     string
	SizeBytes int64
	Seeders   int
}

// RankCandidates orders candidates by seeding urgency: fewest seeds first,
// with smaller torrents preferred as a tie-break (they're cheaper to try
// first while keep-at works down the list). Candidates that aren't Available
// are excluded entirely, since they'd stall a download indefinitely.
//
// ramBound, when true, means keep-at is already at its RAM-driven torrent
// cap, so the binding constraint is the per-torrent RAM slot rather than
// disk. The size tie-break then flips to prefer LARGER torrents, because each
// RAM slot seeds a fixed overhead regardless of size - a big torrent fills a
// slot far more efficiently than a small one, which is exactly what a
// RAM-constrained, disk-rich host (e.g. a 1 GB Pi on a 20 TB disk) wants.
func RankCandidates(candidates []Candidate, ramBound bool) []Candidate {
	ranked := make([]Candidate, 0, len(candidates))
	for _, c := range candidates {
		if c.Available() {
			ranked = append(ranked, c)
		}
	}
	sort.SliceStable(ranked, func(i, j int) bool {
		if ranked[i].Seeders != ranked[j].Seeders {
			return ranked[i].Seeders < ranked[j].Seeders
		}
		if ramBound {
			return ranked[i].SizeBytes > ranked[j].SizeBytes
		}
		return ranked[i].SizeBytes < ranked[j].SizeBytes
	})
	return ranked
}

// AntiCascadeChance is n: the probability keep-at proceeds with a
// candidate given how many other keep-at nodes are already on it. See
// DESIGN.md for the full rationale.
//
// With zero other keep-at nodes present, n is 1: go ahead confidently. As
// more keep-at nodes join the swarm, n shrinks toward zero, since
// aggressiveness is strictly between 0 and 1. This is the direction that
// actually prevents a cascade - later keep-at nodes progressively back off
// from a torrent the swarm has already piled onto.
func AntiCascadeChance(aggressiveness float64, keepAtPeers int) float64 {
	if keepAtPeers < 0 {
		keepAtPeers = 0
	}
	return math.Pow(aggressiveness, float64(keepAtPeers))
}

// MeetsSeedMargin reports whether candidateSeeders is at least minSeedMargin
// lower than every torrent in displaced. It's cheap (no swarm probing
// involved) and deliberately checkable before EvaluateSwap so callers can
// skip an expensive keep-at-peer probe for candidates that would fail this
// check anyway. An empty displaced list always passes: there's nothing to
// beat when keep-at is just filling free space, not swapping.
func MeetsSeedMargin(candidateSeeders int, displaced []Held, minSeedMargin int) bool {
	if len(displaced) == 0 {
		return true
	}
	minDisplacedSeeders := displaced[0].Seeders
	for _, d := range displaced[1:] {
		if d.Seeders < minDisplacedSeeders {
			minDisplacedSeeders = d.Seeders
		}
	}
	return candidateSeeders <= minDisplacedSeeders-minSeedMargin
}

// SwapDecision is the outcome of evaluating whether to swap to a candidate.
type SwapDecision struct {
	ShouldSwap bool
	Chance     float64 // n
	Roll       float64
	Reason     string
}

// EvaluateSwap decides whether to start downloading candidate, optionally
// displacing the torrents in displaced to make room. It swaps when roll <
// n: see DESIGN.md for why that comparison direction, not the reverse, is
// what actually discourages a cascade.
//
// roll must be a fresh uniform [0, 1) draw per call; it's a parameter
// rather than generated internally so this stays deterministic to test.
func EvaluateSwap(candidate Candidate, displaced []Held, minSeedMargin int, aggressiveness float64, roll float64) SwapDecision {
	if !candidate.Available() {
		return SwapDecision{ShouldSwap: false, Reason: "candidate has no live seed"}
	}

	if !MeetsSeedMargin(candidate.Seeders, displaced, minSeedMargin) {
		return SwapDecision{ShouldSwap: false, Reason: "candidate does not beat displaced torrents by the required margin"}
	}

	chance := AntiCascadeChance(aggressiveness, candidate.KeepAtPeers)
	shouldSwap := roll < chance

	reason := "anti-cascade roll succeeded"
	if !shouldSwap {
		reason = "anti-cascade roll failed; backing off while other keep-at nodes are already on this torrent"
	}

	return SwapDecision{ShouldSwap: shouldSwap, Chance: chance, Roll: roll, Reason: reason}
}
