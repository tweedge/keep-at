// Package selector implements mimis' two central decisions: which torrent
// is most urgently in need of seeding, and whether swapping to it right now
// would risk a cascade (every mimis node globally piling onto the same
// under-seeded torrent at once).
package selector

import (
	"math"
	"sort"

	"github.com/anacrolix/torrent/metainfo"
)

// Candidate is a torrent mimis is considering downloading, combining
// catalog metadata with a fresh tracker scrape.
type Candidate struct {
	InfoHash   metainfo.Hash
	Title      string
	SizeBytes  int64
	Seeders    int
	Leechers   int
	MimisPeers int // peers in the swarm that self-identified as mimis, see buildinfo.ClientName
}

// Available reports whether a candidate meets the availability bar: at
// least one live seed. The plan also allows "peers with sufficient data"
// (i.e. leechers whose combined pieces cover 100% of the torrent even with
// zero full seeds) to count as available, but verifying that requires
// per-peer piece-map inspection across the whole swarm, which is expensive
// to do for every catalog candidate during a scan. mimis uses the cheaper,
// conservative signal instead: a torrent isn't a candidate until someone
// can serve 100% of it outright.
func (c Candidate) Available() bool {
	return c.Seeders >= 1
}

// Held is a torrent mimis currently stores and seeds.
type Held struct {
	InfoHash  metainfo.Hash
	Title     string
	SizeBytes int64
	Seeders   int
}

// RankCandidates orders candidates by seeding urgency: fewest seeds first,
// with smaller torrents preferred as a tie-break (they're cheaper to try
// first while mimis works down the list). Candidates that aren't Available
// are excluded entirely, since they'd stall a download indefinitely.
func RankCandidates(candidates []Candidate) []Candidate {
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
		return ranked[i].SizeBytes < ranked[j].SizeBytes
	})
	return ranked
}

// AntiCascadeChance is n from PLAN.md: the probability mimis proceeds with
// a candidate given how many other mimis nodes are already on it.
//
// With zero other mimis nodes present, n is 1: go ahead confidently. As
// more mimis nodes join the swarm, n shrinks toward zero, since
// aggressiveness is strictly between 0 and 1. This is the direction that
// actually prevents a cascade - later mimis nodes progressively back off
// from a torrent the swarm has already piled onto. (PLAN.md's own text
// describes rolling "higher than n" to swap, which would do the opposite -
// see EvaluateSwap's doc comment for why mimis rolls the other way.)
func AntiCascadeChance(aggressiveness float64, mimisPeers int) float64 {
	if mimisPeers < 0 {
		mimisPeers = 0
	}
	return math.Pow(aggressiveness, float64(mimisPeers))
}

// MeetsSeedMargin reports whether candidateSeeders is at least minSeedMargin
// lower than every torrent in displaced. It's cheap (no swarm probing
// involved) and deliberately checkable before EvaluateSwap so callers can
// skip an expensive mimis-peer probe for candidates that would fail this
// check anyway. An empty displaced list always passes: there's nothing to
// beat when mimis is just filling free space, not swapping.
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
// displacing the torrents in displaced to make room.
//
// PLAN.md defines n as "the chance that mimis swaps to the new torrent,"
// which only prevents a global cascade if the roll succeeds *more* often
// when n is *larger*. Its literal wording ("if mimis rolls higher than n,
// swap") does the opposite: n shrinks as more mimis nodes join a swarm, so
// rolling above a shrinking threshold gets easier over time, encouraging
// exactly the pile-on the mechanism exists to prevent. mimis implements the
// definition ("n = chance mimis swaps") rather than the inverted
// comparison: it swaps when roll < n.
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

	chance := AntiCascadeChance(aggressiveness, candidate.MimisPeers)
	shouldSwap := roll < chance

	reason := "anti-cascade roll succeeded"
	if !shouldSwap {
		reason = "anti-cascade roll failed; backing off while other mimis nodes are already on this torrent"
	}

	return SwapDecision{ShouldSwap: shouldSwap, Chance: chance, Roll: roll, Reason: reason}
}
