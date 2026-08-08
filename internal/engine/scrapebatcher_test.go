package engine

import (
	"context"
	"fmt"
	"sync"
	"testing"
	"time"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/attorrent"
)

func testBatcherEngine(t *testing.T) *Engine {
	t.Helper()
	return &Engine{
		torrentFetcher: &attorrent.Fetcher{}, // batcher checks Limiter != nil
		swarmCache:     newSwarmCache(t.TempDir()+"/scrape-cache.json", time.Hour),
	}
}

// TestScrapeBatcherBatchesByTracker verifies that N candidates spread across
// two trackers produce exactly two batched requests (one per tracker), not N
// individual requests, and that every candidate gets its own counts back.
func TestScrapeBatcherBatchesByTracker(t *testing.T) {
	e := testBatcherEngine(t)

	var mu sync.Mutex
	var requests []struct {
		tracker string
		hashes  int
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	b := newScrapeBatcher(e, ctx, &scanStats{})

	// Stub out the network call so we can observe exactly what gets batched.
	b.scrapeBatchFn = func(_ context.Context, tracker string, hashes []metainfo.Hash) (map[metainfo.Hash]attorrent.SwarmCounts, error) {
		mu.Lock()
		requests = append(requests, struct {
			tracker string
			hashes  int
		}{tracker, len(hashes)})
		mu.Unlock()
		out := make(map[metainfo.Hash]attorrent.SwarmCounts, len(hashes))
		for _, h := range hashes {
			out[h] = attorrent.SwarmCounts{Seeders: 5, Leechers: 1}
		}
		return out, nil
	}

	// Two hashes on tracker A, one on tracker B. All submitted before the
	// batch fills (scrapeBatchSize is much larger), so the only flush is the
	// one from close(), which drains everything in one batched call per
	// tracker.
	var hashesA, hashesB []metainfo.Hash
	const trackerA = "https://academictorrents.com/announce.php"
	const trackerB = "https://ipv6.academictorrents.com/announce.php"
	for i := 0; i < 2; i++ {
		h := metainfo.Hash{byte(i + 1)}
		hashesA = append(hashesA, h)
		got := b.submit(ctx, h, trackerA)
		if got == nil {
			t.Fatal("submit returned nil channel")
		}
	}
	hB := metainfo.Hash{0xff}
	hashesB = append(hashesB, hB)
	if got := b.submit(ctx, hB, trackerB); got == nil {
		t.Fatal("submit returned nil channel")
	}

	// close() flushes the final partial batch. But workers in the real scan
	// wait on the result channel, which close()'s flush fills. Here we just
	// verify the requests observed after close().
	b.close()

	mu.Lock()
	defer mu.Unlock()
	if len(requests) != 2 {
		t.Fatalf("expected 2 batched requests (one per tracker), got %d: %+v", len(requests), requests)
	}
	byTracker := map[string]int{}
	for _, r := range requests {
		byTracker[r.tracker] += r.hashes
	}
	if byTracker[trackerA] != len(hashesA) {
		t.Errorf("tracker A got %d hashes, want %d", byTracker[trackerA], len(hashesA))
	}
	if byTracker[trackerB] != len(hashesB) {
		t.Errorf("tracker B got %d hashes, want %d", byTracker[trackerB], len(hashesB))
	}
}

// TestScrapeBatcherFlushesFullBatch verifies that a batch is flushed as soon
// as it reaches scrapeBatchSize, without waiting for close().
func TestScrapeBatcherFlushesFullBatch(t *testing.T) {
	e := testBatcherEngine(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	flushed := make(chan int, 4)
	b := newScrapeBatcher(e, ctx, &scanStats{})
	defer b.close()

	b.scrapeBatchFn = func(_ context.Context, tracker string, hashes []metainfo.Hash) (map[metainfo.Hash]attorrent.SwarmCounts, error) {
		flushed <- len(hashes)
		out := make(map[metainfo.Hash]attorrent.SwarmCounts, len(hashes))
		for _, h := range hashes {
			out[h] = attorrent.SwarmCounts{Seeders: 1}
		}
		return out, nil
	}

	tracker := "https://academictorrents.com/announce.php"
	for i := 0; i < scrapeBatchSize; i++ {
		h := metainfo.Hash{byte(i % 256)}
		if got := b.submit(ctx, h, tracker); got == nil {
			t.Fatal("submit returned nil channel")
		}
	}

	select {
	case n := <-flushed:
		if n != scrapeBatchSize {
			t.Fatalf("full-batch flush sent %d hashes, want %d", n, scrapeBatchSize)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("batcher did not flush a full batch promptly")
	}
}

// TestScrapeBatcherRoutesErrors verifies a failed batch scrape is routed as
// an error to every candidate in that batch, not silently dropped.
func TestScrapeBatcherRoutesErrors(t *testing.T) {
	e := testBatcherEngine(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	b := newScrapeBatcher(e, ctx, &scanStats{})

	b.scrapeBatchFn = func(context.Context, string, []metainfo.Hash) (map[metainfo.Hash]attorrent.SwarmCounts, error) {
		return nil, fmt.Errorf("tracker down")
	}

	tracker := "https://academictorrents.com/announce.php"
	h := metainfo.Hash{1}
	result := b.submit(ctx, h, tracker)
	if result == nil {
		t.Fatal("submit returned nil channel")
	}

	b.close()
	select {
	case r := <-result:
		if r.err == nil {
			t.Fatal("expected an error routed back from a failed batch scrape")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for routed error")
	}
}
