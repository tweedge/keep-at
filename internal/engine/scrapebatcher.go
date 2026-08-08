package engine

import (
	"context"
	"fmt"
	"time"

	"github.com/anacrolix/torrent/metainfo"

	"github.com/tweedge/keep-at/internal/attorrent"
	"github.com/tweedge/keep-at/internal/buildinfo"
)

// scrapeBatchSize is how many candidate hashes keep-at groups into one
// batched tracker scrape request. Batching is what makes the scrape phase
// fast: a full-catalog scan would otherwise issue one rate-limited HTTP
// request per candidate (thousands of requests, each costing a 2-second
// limiter wait at the default 0.5/s), whereas one batched request covers
// scrapeBatchSize candidates with a single limiter wait and a single HTTP
// round trip.
const scrapeBatchSize = 64

// scrapeBatchInterval is the longest keep-at will hold a partial batch before
// flushing it anyway, so the last few candidates of a scan (or a trickle of
// stragglers) don't sit waiting for a full batch to accumulate. It also sets
// how quickly candidates start flowing out of evaluation: each flush emits up
// to scrapeBatchSize evaluated candidates at once.
const scrapeBatchInterval = 500 * time.Millisecond

// scrapeJob is one candidate's tracker scrape, submitted to the batcher and
// answered on result when its batch is flushed.
type scrapeJob struct {
	infoHash metainfo.Hash
	tracker  string // the AT-hosted tracker URL to scrape against
	result   chan scrapeResult
}

// scrapeResult carries the outcome of one candidate's scrape back to the
// evaluating worker. err is non-nil when the tracker returned nothing usable
// for this hash (a dead/missing tracker or a malformed response), matching
// what scrapeSwarm would have returned as a hard failure.
type scrapeResult struct {
	counts attorrent.SwarmCounts
	err    error
}

// scrapeBatcher groups candidate tracker scrapes into batched multi-hash
// requests (BEP 48) so the scrape phase issues one rate-limited request per
// scrapeBatchSize candidates instead of one per candidate. Scrapes only ever
// target Academic Torrents' own tracker hosts: AT is authoritative for AT
// torrents, and third-party trackers in a .torrent's list are mostly dead
// and were the source of the old per-candidate 15-second timeouts.
//
// One batcher lives for one scan. It runs its own goroutine (run), workers
// submit jobs via submit and block on the returned result channel, and the
// caller closes the input and waits on Done when evaluation finishes so the
// final partial batch is flushed before the scan's results channel closes.
type scrapeBatcher struct {
	e     *Engine
	ctx   context.Context
	stats *scanStats

	// scrapeBatchFn issues one batched multi-hash scrape to tracker and
	// returns per-hash counts. It's a field so tests can substitute a stub
	// that records the exact batched requests without touching the network;
	// the production value is batchedScrape.
	scrapeBatchFn func(ctx context.Context, tracker string, hashes []metainfo.Hash) (map[metainfo.Hash]attorrent.SwarmCounts, error)

	in  chan scrapeJob
	done chan struct{}

	// pending accumulates not-yet-flushed jobs, keyed by tracker URL so each
	// tracker gets its own batched request. Only touched by the run goroutine.
	pending map[string][]scrapeJob
}

// batchedScrape issues one BEP 48 multi-hash scrape to a tracker, covering
// every pending hash for that tracker in a single rate-limited request.
func (e *Engine) batchedScrape(ctx context.Context, tracker string, hashes []metainfo.Hash) (map[metainfo.Hash]attorrent.SwarmCounts, error) {
	return attorrent.Scrape(ctx, e.httpClient, e.udpScraper, buildinfo.ScraperUserAgent(), tracker, hashes)
}

// newScrapeBatcher starts a batcher for one scan and launches its goroutine.
// inBufSize is how many jobs may be queued ahead of the batcher before
// workers block; it's sized to absorb a full evaluation worker set plus the
// progress of one in-flight batch so workers rarely stall on submission.
func newScrapeBatcher(e *Engine, ctx context.Context, stats *scanStats) *scrapeBatcher {
	b := &scrapeBatcher{
		e:             e,
		ctx:           ctx,
		stats:         stats,
		scrapeBatchFn: e.batchedScrape,
		in:            make(chan scrapeJob, evaluateConcurrency+scrapeBatchSize),
		done:          make(chan struct{}),
		pending:       make(map[string][]scrapeJob),
	}
	go b.run()
	return b
}

// submit queues one candidate's scrape and returns the channel its result
// will arrive on. Returns nil if ctx is cancelled before the job is accepted.
func (b *scrapeBatcher) submit(ctx context.Context, infoHash metainfo.Hash, tracker string) <-chan scrapeResult {
	job := scrapeJob{
		infoHash: infoHash,
		tracker:  tracker,
		result:   make(chan scrapeResult, 1),
	}
	select {
	case b.in <- job:
		return job.result
	case <-ctx.Done():
		return nil
	}
}

// run is the batcher's main loop: accumulate submitted jobs, flush when a
// batch fills or the interval elapses, and drain everything (including the
// final partial batch) when the input is closed or ctx is cancelled.
func (b *scrapeBatcher) run() {
	defer close(b.done)

	ticker := time.NewTicker(scrapeBatchInterval)
	defer ticker.Stop()

	for {
		select {
		case <-b.ctx.Done():
			b.flush()
			return
		case job, ok := <-b.in:
			if !ok {
				// Input closed (close): flush whatever is still pending and
				// exit. Without the ok check, <-b.in keeps returning zero
				// values after close and the loop would spin forever.
				b.flush()
				return
			}
			b.pending[job.tracker] = append(b.pending[job.tracker], job)
			if b.countPending() >= scrapeBatchSize {
				b.flush()
			}
		case <-ticker.C:
			b.flush()
		}
	}
}

// close stops the batcher: it flushes whatever is still pending and waits for
// the run goroutine to exit. Must be called exactly once, after all workers
// have finished submitting.
func (b *scrapeBatcher) close() {
	close(b.in)
	<-b.done
}

func (b *scrapeBatcher) countPending() int {
	total := 0
	for _, jobs := range b.pending {
		total += len(jobs)
	}
	return total
}

// flush issues one batched scrape request per pending tracker URL and routes
// each candidate's result back to its worker. A single limiter wait covers
// the whole batch; that shared limiter is the politeness budget to
// academictorrents.com, so amortizing it over scrapeBatchSize candidates is
// exactly what makes the scrape phase fast.
func (b *scrapeBatcher) flush() {
	if b.countPending() == 0 {
		return
	}

	pending := b.pending
	b.pending = make(map[string][]scrapeJob)

	for tracker, jobs := range pending {
		b.scrapeBatch(tracker, jobs)
	}
}

func (b *scrapeBatcher) scrapeBatch(tracker string, jobs []scrapeJob) {
	hashes := make([]metainfo.Hash, 0, len(jobs))
	for _, j := range jobs {
		hashes = append(hashes, j.infoHash)
	}

	if b.e.torrentFetcher.Limiter != nil {
		if err := b.e.torrentFetcher.Limiter.Wait(b.ctx); err != nil {
			for _, j := range jobs {
				j.result <- scrapeResult{err: err}
			}
			return
		}
	}
	if b.stats != nil {
		b.stats.scrapeRequests.Add(1)
	}

	callCtx, cancel := context.WithTimeout(b.ctx, scrapeTimeout)
	counts, err := b.scrapeBatchFn(callCtx, tracker, hashes)
	cancel()

	for _, j := range jobs {
		if err == nil {
			if c, ok := counts[j.infoHash]; ok {
				b.e.swarmCache.put(j.infoHash, c)
				j.result <- scrapeResult{counts: c}
				continue
			}
			err = fmt.Errorf("engine: tracker %s returned no data for %s", tracker, j.infoHash.HexString())
		}
		j.result <- scrapeResult{err: err}
	}
}
