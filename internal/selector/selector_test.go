package selector

import (
	"math"
	"testing"
)

func TestSelectionChance(t *testing.T) {
	cases := []struct {
		name           string
		aggressiveness float64
		seeders        int
		floor          int
		want           float64
	}{
		// floor 1 (or 0, which clamps to 1) = the original formula
		{"single seeder is the primary target", 0.6, 1, 1, 1.0},
		{"two seeders", 0.6, 2, 1, 0.6},
		{"three seeders", 0.6, 3, 1, 0.36},
		{"seeders clamped to at least one", 0.6, 0, 1, 1.0},
		{"seeders clamped to at least one (negative)", 0.6, -5, 1, 1.0},
		{"floor 0 clamps to 1, same as floor 1", 0.6, 2, 0, 0.6},

		// floor 3: a candidate at or below the floor passes confidently
		{"at floor passes with full confidence", 0.6, 3, 3, 1.0},
		{"below floor passes with full confidence", 0.6, 1, 3, 1.0},
		{"one above floor", 0.6, 4, 3, 0.6},
		{"two above floor", 0.6, 5, 3, 0.36},

		// floor 2: two seeders are no longer special-cased, they're healthy
		{"healthier floor: two seeders at floor", 0.6, 2, 2, 1.0},
		{"healthier floor: three seeders one above", 0.6, 3, 2, 0.6},
	}
	for _, c := range cases {
		got := SelectionChance(c.aggressiveness, c.seeders, c.floor)
		if math.Abs(got-c.want) > 1e-9 {
			t.Errorf("%s: SelectionChance(%v, %d, %d) = %v, want %v", c.name, c.aggressiveness, c.seeders, c.floor, got, c.want)
		}
	}
}

func TestSelectionChanceDecreasesAsSeedersIncrease(t *testing.T) {
	// keep-at exists to seed minimally-seeded torrents: every additional
	// seeder makes a torrent more healthy on its own, so it should be
	// strictly less likely (or at least no more likely) that keep-at selects
	// it. A torrent with many seeders is effectively never selected. With a
	// floor of 1 this reduces to the original formula, where the decay starts
	// immediately.
	aggressiveness := 0.6
	prev := SelectionChance(aggressiveness, 1, 1)
	for n := 2; n <= 20; n++ {
		cur := SelectionChance(aggressiveness, n, 1)
		if cur >= prev {
			t.Fatalf("chance did not decrease at seeders=%d: prev=%v cur=%v", n, prev, cur)
		}
		prev = cur
	}
}

func TestSelectionChanceWithFloorIsFlatBelowFloorThenDecreases(t *testing.T) {
	// A higher floor means the catalog as a whole is healthier, so torrents
	// at or below the floor are all treated as primary targets (chance 1.0),
	// and decay only starts above it.
	aggressiveness := 0.6
	floor := 3
	for n := 1; n <= floor; n++ {
		if got := SelectionChance(aggressiveness, n, floor); got != 1.0 {
			t.Fatalf("chance at or below floor = %v, want 1.0 (seeders=%d)", got, n)
		}
	}
	prev := 1.0
	for n := floor + 1; n <= floor+20; n++ {
		cur := SelectionChance(aggressiveness, n, floor)
		if cur >= prev {
			t.Fatalf("chance did not decrease above floor at seeders=%d: prev=%v cur=%v", n, prev, cur)
		}
		prev = cur
	}
}

func TestSeederFloor(t *testing.T) {
	cases := []struct {
		name string
		in   []int
		want int
	}{
		{"nothing seeded", []int{0, 0}, 0},
		{"empty", []int{}, 0},
		{"single seeded torrent", []int{1}, 1},
		{"all at one", []int{1, 1, 1, 1}, 1},
		{"ten torrents, p10 is the minimum", []int{1, 1, 2, 2, 3, 3, 4, 4, 5, 6}, 1},
		{"twenty torrents, p10 is the second-smallest", []int{1, 1, 2, 2, 3, 3, 4, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16}, 1},
		{"zero-seeder torrents ignored", []int{0, 0, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5}, 5},
		{"unsorted input", []int{6, 1, 4, 2, 3, 5}, 1},
	}
	for _, c := range cases {
		if got := SeederFloor(c.in); got != c.want {
			t.Errorf("%s: SeederFloor(%v) = %d, want %d", c.name, c.in, got, c.want)
		}
	}
}

func TestRankCandidatesOrdersBySeedersThenSize(t *testing.T) {
	candidates := []Candidate{
		{Title: "big-lowseed", Seeders: 1, SizeBytes: 1000},
		{Title: "small-lowseed", Seeders: 1, SizeBytes: 10},
		{Title: "unavailable", Seeders: 0, SizeBytes: 1},
		{Title: "highseed", Seeders: 50, SizeBytes: 1},
	}

	ranked := RankCandidates(candidates, false)
	if len(ranked) != 3 {
		t.Fatalf("expected unavailable candidate excluded, got %d results: %+v", len(ranked), ranked)
	}
	wantOrder := []string{"small-lowseed", "big-lowseed", "highseed"}
	for i, want := range wantOrder {
		if ranked[i].Title != want {
			t.Errorf("position %d: got %q, want %q", i, ranked[i].Title, want)
		}
	}
}

func TestRankCandidatesRamBoundPrefersLarger(t *testing.T) {
	candidates := []Candidate{
		{Title: "big-lowseed", Seeders: 1, SizeBytes: 1000},
		{Title: "small-lowseed", Seeders: 1, SizeBytes: 10},
		{Title: "unavailable", Seeders: 0, SizeBytes: 1},
		{Title: "highseed", Seeders: 50, SizeBytes: 1},
	}

	ranked := RankCandidates(candidates, true)
	if len(ranked) != 3 {
		t.Fatalf("expected unavailable candidate excluded, got %d results: %+v", len(ranked), ranked)
	}
	// RAM-bound: among equal seeders, the larger torrent wins the scarce
	// per-torrent RAM slot.
	wantOrder := []string{"big-lowseed", "small-lowseed", "highseed"}
	for i, want := range wantOrder {
		if ranked[i].Title != want {
			t.Errorf("position %d: got %q, want %q", i, ranked[i].Title, want)
		}
	}
}

func TestEvaluateSwapRejectsUnavailableCandidate(t *testing.T) {
	candidate := Candidate{Seeders: 0}
	decision := EvaluateSwap(candidate, nil, 3, 0.6, 0.0)
	if decision.ShouldSwap {
		t.Fatalf("expected no swap for unavailable candidate")
	}
}

func TestEvaluateSwapEnforcesSeedMargin(t *testing.T) {
	candidate := Candidate{Seeders: 5}
	displaced := []Held{{Seeders: 6}} // margin of 3 required, only 1 point better

	decision := EvaluateSwap(candidate, displaced, 3, 0.6, 0.0)
	if decision.ShouldSwap {
		t.Fatalf("expected margin check to block swap, got %+v", decision)
	}
}

func TestEvaluateSwapAllowsSwapWhenMarginMetAndRollSucceeds(t *testing.T) {
	candidate := Candidate{Seeders: 1, KeepAtPeers: 0} // chance = 1.0 with one seeder
	displaced := []Held{{Seeders: 10}, {Seeders: 8}}   // min displaced = 8, margin 3 -> need <= 5

	decision := EvaluateSwap(candidate, displaced, 3, 0.6, 0.999)
	if !decision.ShouldSwap {
		t.Fatalf("expected swap to proceed, got %+v", decision)
	}
	if decision.Chance != 1.0 {
		t.Fatalf("expected chance 1.0 with a single seeder, got %v", decision.Chance)
	}
}

func TestEvaluateSwapBacksOffWhenManySeedersPresent(t *testing.T) {
	// keep-at's purpose is to seed minimally-seeded torrents. A candidate
	// with many seeders is already healthy on its own and should effectively
	// never be selected, regardless of the keep-at peer count.
	candidate := Candidate{Seeders: 12, KeepAtPeers: 0}
	// chance = 0.6^11 ~= 0.0036; a roll of 0.5 should fail to swap.
	decision := EvaluateSwap(candidate, nil, 3, 0.6, 0.5)
	if decision.ShouldSwap {
		t.Fatalf("expected swap to be rejected for a well-seeded torrent, got %+v", decision)
	}
}

func TestEvaluateSwapWithNoDisplacementSkipsMarginCheckButStillGatesOnSeeders(t *testing.T) {
	// Used for filling free space, not just swapping - no torrents to beat,
	// so the margin check passes. But the seed-scarcity gate still applies:
	// a torrent with many seeders shouldn't be filled just because space is
	// free.
	candidate := Candidate{Seeders: 100, KeepAtPeers: 0}
	decision := EvaluateSwap(candidate, nil, 3, 0.6, 0.5)
	if decision.ShouldSwap {
		t.Fatalf("expected well-seeded torrent to be rejected even with free space, got %+v", decision)
	}
	if !decision.SeedScarcityBlocked() {
		t.Fatalf("expected a seed-scarcity roll failure to be flagged, got %+v", decision)
	}
}

func TestEvaluateSwapUsesSeederFloor(t *testing.T) {
	// A candidate at or below the catalog's p10 floor passes with full
	// confidence even with a high roll, and one above the floor backs off.
	cases := []struct {
		name       string
		seeders    int
		floor      int
		roll       float64
		shouldSwap bool
		blocked    bool
	}{
		{"at floor passes with full confidence", 3, 3, 0.99, true, false},
		{"below floor passes with full confidence", 1, 5, 0.99, true, false},
		{"above floor backs off on a bad roll", 5, 2, 0.99, false, true},
		{"above floor passes on a good roll", 3, 1, 0.01, true, false},
		{"no floor yet means floor 1", 1, 0, 0.99, true, false},
	}
	for _, c := range cases {
		cand := Candidate{Seeders: c.seeders, SeederFloor: c.floor}
		decision := EvaluateSwap(cand, nil, 0, 0.6, c.roll)
		if decision.ShouldSwap != c.shouldSwap {
			t.Errorf("%s: EvaluateSwap(...) ShouldSwap = %v, want %v (decision %+v)", c.name, decision.ShouldSwap, c.shouldSwap, decision)
		}
		if decision.SeedScarcityBlocked() != c.blocked {
			t.Errorf("%s: SeedScarcityBlocked() = %v, want %v (decision %+v)", c.name, decision.SeedScarcityBlocked(), c.blocked, decision)
		}
	}
}

func TestSeedScarcityBlockedOnlyForRollFailures(t *testing.T) {
	// Other rejection reasons - unavailable, margin not met - are not
	// seed-scarcity roll failures and must not be suppressed as if they were.
	unavailable := EvaluateSwap(Candidate{Seeders: 0}, nil, 3, 0.6, 0.0)
	if unavailable.SeedScarcityBlocked() {
		t.Fatalf("unavailable candidate misflagged as seed-scarcity block: %+v", unavailable)
	}
	margin := EvaluateSwap(Candidate{Seeders: 5}, []Held{{Seeders: 6}}, 3, 0.6, 0.0)
	if margin.SeedScarcityBlocked() {
		t.Fatalf("margin failure misflagged as seed-scarcity block: %+v", margin)
	}
	ok := EvaluateSwap(Candidate{Seeders: 1}, nil, 3, 0.6, 0.01)
	if ok.SeedScarcityBlocked() {
		t.Fatalf("successful swap misflagged as seed-scarcity block: %+v", ok)
	}
}
