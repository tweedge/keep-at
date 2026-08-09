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
		want           float64
	}{
		{"single seeder is the primary target", 0.6, 1, 1.0},
		{"two seeders", 0.6, 2, 0.6},
		{"three seeders", 0.6, 3, 0.36},
		{"seeders clamped to at least one", 0.6, 0, 1.0},
		{"seeders clamped to at least one (negative)", 0.6, -5, 1.0},
	}
	for _, c := range cases {
		got := SelectionChance(c.aggressiveness, c.seeders)
		if math.Abs(got-c.want) > 1e-9 {
			t.Errorf("%s: SelectionChance(%v, %d) = %v, want %v", c.name, c.aggressiveness, c.seeders, got, c.want)
		}
	}
}

func TestSelectionChanceDecreasesAsSeedersIncrease(t *testing.T) {
	// keep-at exists to seed minimally-seeded torrents: every additional
	// seeder makes a torrent more healthy on its own, so it should be
	// strictly less likely (or at least no more likely) that keep-at selects
	// it. A torrent with many seeders is effectively never selected.
	aggressiveness := 0.6
	prev := SelectionChance(aggressiveness, 1)
	for n := 2; n <= 20; n++ {
		cur := SelectionChance(aggressiveness, n)
		if cur >= prev {
			t.Fatalf("chance did not decrease at seeders=%d: prev=%v cur=%v", n, prev, cur)
		}
		prev = cur
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
}
