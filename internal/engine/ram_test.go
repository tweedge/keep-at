package engine

import "testing"

func TestRamBudgetClampsToHardCap(t *testing.T) {
	const perTorrent = int64(4 << 20) // 4 MiB
	const systemRAM = int64(16 << 30)
	hardCapF := 0.8 * float64(systemRAM)
	wantHardCap := int64(hardCapF)



	// No explicit max: spend the full hard cap of 80% of system RAM.
	budget, hardCap, maxTorrents := ramBudget(systemRAM, 0, 0, perTorrent)
	if budget != wantHardCap {
		t.Errorf("budget = %d, want hard cap %d", budget, wantHardCap)
	}
	if hardCap != wantHardCap {
		t.Errorf("hardCap = %d, want %d", hardCap, wantHardCap)
	}
	if maxTorrents != int(wantHardCap/perTorrent) {
		t.Errorf("maxTorrents = %d, want %d", maxTorrents, wantHardCap/perTorrent)
	}

	// Explicit max above the hard cap is clamped down to the cap.
	budget, hardCap, maxTorrents = ramBudget(systemRAM, 15<<30, 0, perTorrent)
	if budget != wantHardCap {
		t.Errorf("budget with oversized max = %d, want clamped %d", budget, wantHardCap)
	}
	if hardCap != wantHardCap {
		t.Errorf("hardCap = %d, want %d", hardCap, wantHardCap)
	}

	// Explicit max within the cap is honored.
	const wantBudget = int64(2 << 30)
	budget, _, maxTorrents = ramBudget(systemRAM, wantBudget, 0, perTorrent)
	if budget != wantBudget {
		t.Errorf("budget = %d, want 2GiB", budget)
	}
	if maxTorrents != int(wantBudget/perTorrent) {
		t.Errorf("maxTorrents = %d, want %d", maxTorrents, wantBudget/perTorrent)
	}

	// Config max (no --max-ram) is honored, and still clamped by the cap.
	const configMax = int64(1 << 30)
	budget, _, _ = ramBudget(systemRAM, 0, configMax, perTorrent)
	if budget != configMax {
		t.Errorf("config-max budget = %d, want 1GiB", budget)
	}
}

func TestPerTorrentConnRAM(t *testing.T) {
	want := int64(establishedConnsPerTorrent) * int64(maxAllocPeerRequestData)
	if got := PerTorrentConnRAM(establishedConnsPerTorrent, maxAllocPeerRequestData); got != want {
		t.Errorf("PerTorrentConnRAM = %d, want %d", got, want)
	}
}
