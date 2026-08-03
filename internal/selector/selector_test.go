package selector

import (
	"math"
	"testing"
)

func TestAntiCascadeChance(t *testing.T) {
	cases := []struct {
		name           string
		aggressiveness float64
		keepAtPeers    int
		want           float64
	}{
		{"no other keep-at nodes", 0.6, 0, 1.0},
		{"one other node", 0.6, 1, 0.6},
		{"two other nodes", 0.6, 2, 0.36},
		{"negative peers clamped to zero", 0.6, -5, 1.0},
	}
	for _, c := range cases {
		got := AntiCascadeChance(c.aggressiveness, c.keepAtPeers)
		if math.Abs(got-c.want) > 1e-9 {
			t.Errorf("%s: AntiCascadeChance(%v, %d) = %v, want %v", c.name, c.aggressiveness, c.keepAtPeers, got, c.want)
		}
	}
}

func TestAntiCascadeChanceDecreasesAsNodesJoin(t *testing.T) {
	// This is the whole point of the mechanism: each additional keep-at node
	// already on a torrent should make it strictly less likely (or at least
	// no more likely) that yet another node piles on.
	aggressiveness := 0.6
	prev := AntiCascadeChance(aggressiveness, 0)
	for n := 1; n <= 20; n++ {
		cur := AntiCascadeChance(aggressiveness, n)
		if cur >= prev {
			t.Fatalf("chance did not decrease at keepAtPeers=%d: prev=%v cur=%v", n, prev, cur)
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

	ranked := RankCandidates(candidates)
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
	candidate := Candidate{Seeders: 1, KeepAtPeers: 0} // chance = 1.0 with 0 peers
	displaced := []Held{{Seeders: 10}, {Seeders: 8}}   // min displaced = 8, margin 3 -> need <= 5

	decision := EvaluateSwap(candidate, displaced, 3, 0.6, 0.999)
	if !decision.ShouldSwap {
		t.Fatalf("expected swap to proceed, got %+v", decision)
	}
	if decision.Chance != 1.0 {
		t.Fatalf("expected chance 1.0 with zero keep-at peers, got %v", decision.Chance)
	}
}

func TestEvaluateSwapBacksOffWhenManyKeepAtPeersPresent(t *testing.T) {
	candidate := Candidate{Seeders: 1, KeepAtPeers: 10}
	// chance = 0.6^10 ~= 0.006; a roll of 0.5 should fail to swap.
	decision := EvaluateSwap(candidate, nil, 3, 0.6, 0.5)
	if decision.ShouldSwap {
		t.Fatalf("expected swap to be rejected with many keep-at peers already present, got %+v", decision)
	}
}

func TestEvaluateSwapWithNoDisplacementSkipsMarginCheck(t *testing.T) {
	// Used for filling free space, not just swapping - no torrents to beat.
	candidate := Candidate{Seeders: 100, KeepAtPeers: 0}
	decision := EvaluateSwap(candidate, nil, 3, 0.6, 0.5)
	if !decision.ShouldSwap {
		t.Fatalf("expected swap to proceed when nothing is displaced, got %+v", decision)
	}
}
