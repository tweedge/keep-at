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
	InfoHash  metainfo.Hash
	Title     string
	SizeBytes int64
	Seeders   int
	Leechers  int
	// KeepAtPeers is how many other keep-at nodes were observed in the
	// swarm, gathered by probing. It feeds network-status reporting and is
	// logged as metadata; it deliberately does NOT drive the selection
	// decision - keep-at exists to seed minimally-seeded torrents, so the
	// gate below is keyed on total Seeders, not on how many keep-at nodes
	// happen to be present.
	KeepAtPeers int
	// SeederFloor is the p10 (10th percentile) number of seeders across all
	// torrents in the catalog with at least one seeder, as measured by the
	// most recently completed scan. It anchors SelectionChance: a torrent is
	// judged against how the rest of the catalog is doing, not against a
	// fixed baseline of one. Before any scan has completed it's 0, which
	// makes SelectionChance fall back to the original
	// aggressiveness^(seeders-1) behavior. See SelectionChance.
	SeederFloor int
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

// SelectionChance is n: the probability keep-at proceeds with a candidate
// given how many seeders it already has, relative to how the rest of the
// catalog is doing. See DESIGN.md for the full rationale.
//
// keep-at's purpose is to seed minimally-seeded torrents - not to put a
// keep-at copy on everything. The exponent is the candidate's seeder count
// minus the catalog's p10 seeder floor (seederFloor), floored at zero:
//
//	n = aggressiveness ^ max(0, seeders - seederFloor)
//
// With seederFloor 1 (no completed scan yet, or a catalog where a tenth of
// seeded torrents have just one seeder), this reduces to the original
// n = aggressiveness ^ (seeders - 1): a single-seeder torrent is the
// primary target and passes confidently, and n shrinks toward zero as
// seeders accumulate. As the catalog's overall health improves - the p10
// floor rises - so does the seeder count a candidate must beat to be
// confidently selected. That's deliberate: keep-at's job scales with how
// under-seeded the catalog is, and anchoring to a moving floor means it
// keeps finding genuinely under-seeded content as overall health improves,
// while never assuming health has risen above the measured floor before it
// actually has. A torrent at or below the floor is always selected with
// full confidence.
//
// Note this is deliberately keyed on TOTAL seeders, not on how many other
// keep-at nodes are in the swarm. The keep-at peer count is network-status
// metadata (see Candidate.KeepAtPeers); it doesn't gate selection.
func SelectionChance(aggressiveness float64, seeders int, seederFloor int) float64 {
	if seeders < 1 {
		seeders = 1
	}
	if seederFloor < 1 {
		seederFloor = 1
	}
	exponent := seeders - seederFloor
	if exponent < 0 {
		exponent = 0
	}
	return math.Pow(aggressiveness, float64(exponent))
}

// SeederFloor computes the p10 (10th percentile) number of seeders across
// the given torrents' seeder counts, considering only torrents with at
// least one seeder. It's the anchor x for SelectionChance: a candidate at
// or below the floor passes with full confidence, and confidence decays as
// its seeder count rises above the floor.
//
// The nearest-rank method is used: for n positive counts, the floor is the
// value at rank ceil(0.10 * n) when sorted ascending (i.e. the smallest
// value such that at least 10% of seeded torrents have that many or fewer
// seeders). With no positive counts at all - a catalog where nothing is
// seeded - it returns 0, meaning "no health signal", which SelectionChance
// treats as the conservative single-seeder baseline.
func SeederFloor(seedCounts []int) int {
	var seeded []int
	for _, s := range seedCounts {
		if s > 0 {
			seeded = append(seeded, s)
		}
	}
	if len(seeded) == 0 {
		return 0
	}
	sort.Ints(seeded)
	rank := (len(seeded) + 9) / 10 // ceil(n/10), nearest-rank p10
	idx := rank - 1
	if idx < 0 {
		idx = 0
	}
	return seeded[idx]
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

// ReasonSeedScarcityRollFailed is the Reason on a SwapDecision when the
// seed-scarcity roll did not pass: the candidate already has enough seeders
// relative to the catalog's floor, so keep-at backs off. This is the routine
// outcome for well-seeded candidates - the expected case, not an error - and
// the engine deliberately does not log it (see logEvaluatedCandidate).
const ReasonSeedScarcityRollFailed = "seed-scarcity roll failed; candidate already has enough seeders"

// SeedScarcityBlocked reports whether this decision failed specifically
// because the seed-scarcity roll didn't pass, as opposed to some other
// reason (unavailable candidate, seed margin not met, RAM cap, etc.).
func (d SwapDecision) SeedScarcityBlocked() bool {
	return d.Reason == ReasonSeedScarcityRollFailed
}

// EvaluateSwap decides whether to start downloading candidate, optionally
// displacing the torrents in displaced to make room. It swaps when roll <
// n: see DESIGN.md for why that comparison direction is what actually
// keeps keep-at from wasting slots on torrents that are already healthy.
//
// The gate (n) is keyed on the candidate's TOTAL seeders: a torrent with
// one seed is keep-at's primary target and passes confidently, while a
// torrent with many seeds - already healthy on its own - is effectively
// never selected. The keep-at peer count (Candidate.KeepAtPeers) does not
// gate selection; it's network-status metadata only.
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

	chance := SelectionChance(aggressiveness, candidate.Seeders, candidate.SeederFloor)
	shouldSwap := roll < chance

	reason := "seed-scarcity roll succeeded"
	if !shouldSwap {
		reason = ReasonSeedScarcityRollFailed
	}

	return SwapDecision{ShouldSwap: shouldSwap, Chance: chance, Roll: roll, Reason: reason}
}
