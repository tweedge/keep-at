# Design

This is the authoritative explanation of *why* keep-at behaves the way it does: the selection math, the deliberate simplifications, and what's still missing. The README covers how to run it; this covers how it thinks.

## Deciding what to seed

Every scan, keep-at pulls Academic Torrents' catalog (`database.xml`), filters out anything keyword-blocked or too young (see "Age, precisely" below), and checks the remaining candidates' tracker scrape data. A torrent needs at least one live seed to be considered "available" - keep-at won't start a download that can never finish.

Among available candidates, fewer seeds means higher priority. If keep-at has free space, it fills it with the highest-priority candidate it can. If space is full, it'll displace currently-held torrents for a new candidate, but only if the candidate has at least `min_seed_margin` (default 2) fewer seeds than everything it would replace.

## Seeding minimally-seeded torrents

keep-at's purpose is to seed the torrents that need seeding - not to put a keep-at copy on everything. A torrent with one live seed is exactly what keep-at is for; a torrent with a dozen seeders is already healthy on its own and doesn't need keep-at's help. So before committing to a download, keep-at gates on how many seeders the torrent already has, **relative to how healthy the catalog as a whole is**. The chance keep-at proceeds is:

```
n = aggressiveness ^ max(0, seeders - x)
```

where x is the **p10 seeder floor**: the 10th percentile of seeder counts across all catalog torrents the last completed scan saw with at least one seeder, stored in the network-status snapshot (see "Network-wide stats" below).

With a single seeder and no prior data, x is effectively 1, so n is 1: go ahead confidently - that's the primary target. As a torrent gains more seeders, n shrinks toward zero (aggressiveness defaults to 0.6, and is always between 0 and 1), so a torrent with many seeders is effectively never selected. keep-at rolls a random float and proceeds only if the roll is below n.

The floor is what makes this respond to the catalog's overall health rather than a fixed baseline of one:

- **Before any scan has completed** - no network data exists yet - keep-at has never measured the catalog, so x is treated as 1 and the gate reduces to the original `aggressiveness ^ (seeders - 1)`: conservative from the start, and a single-seeder torrent passes confidently.
- **After a scan completes**, x is recomputed from what that scan observed and stored in the network-status snapshot. If overall AcademicTorrents health has improved - the p10 floor is now, say, 2 or 3 - then torrents at or below the new floor are treated as primary targets too (n = 1), and a node with space available keeps finding content to store as its slots fill: it isn't holding back just because the catalog got healthier. Conversely, the floor only rises when health genuinely improves. If it doesn't improve above the measured floor, keep-at stays just as effective at smaller scale - it never assumes the whole network is healthier than it actually is.

This gate runs whether keep-at is filling free space or displacing something, so it applies even when there's plenty of empty disk - a well-seeded torrent isn't worth a slot that could go to one that actually needs it.

The number of other keep-at nodes in a torrent's swarm is deliberately **not** part of this gate. It's network-status data (see "Network-wide stats" below) and is logged as metadata per candidate, but it doesn't change whether keep-at selects a torrent. What matters is how healthy the torrent is overall, which only the total seeder count captures.

## Multi-torrent swaps

A candidate can be bigger than any single held torrent. When that happens, keep-at looks within one storage location for a *set* of held torrents to displace together, not just one:

1. Filter to held torrents that individually clear the seed margin against the candidate (`heldSeeders - min_seed_margin >= candidateSeeders`). A torrent that wouldn't qualify on its own never gets swept in just because it's bundled with others that do.
2. Sort the qualifying torrents by seeders descending - evict the least valuable ones (most-seeded, least in need of keep-at specifically) first.
3. Accumulate them in that order until their combined size covers the candidate. If even every qualifying torrent in that location combined isn't enough room, that location can't take the swap.

This is a greedy selection, not a minimal one - it doesn't search for the smallest possible subset that would fit, just the first prefix (by seeders descending) that does. That keeps the logic simple and predictable at the cost of occasionally evicting one more torrent than a smarter bin-packing solution would need.

## Reasoning quickly about availability (for choosing torrents)

A torrent could theoretically be "available" with zero full seeds, if enough partial peers between them happen to cover 100% of the data. keep-at doesn't attempt to detect that case: verifying it means inspecting every peer's piece map across the whole swarm, which is expensive to do for every catalog candidate on every scan. keep-at uses the cheaper, conservative signal instead - at least one live seed - and accepts that this misses the rare fragmented-but-complete swarm.

**Note: I'm not wedded to this, and might make availability checks more expensive in the future**

## Reasoning conservatively about age (for moderation)

Academic Torrents' `database.xml` doesn't include an upload date, and neither does their API - the closest thing, a paper's publication date, isn't the same thing and would make the moderation delay meaningless. Instead, keep-at reads the `creation date` field baked into each `.torrent` file at fetch time, which is set when the torrent was created (verified against torrents from 2013 through 2026 while building this). A torrent needs to be at least `moderation_delay` old (default 7 days) before keep-at will touch it, giving Academic Torrents' staff time to catch anything that shouldn't be there and boot it off the platform - helping ensure your keep-at node never picks up data you don't want it to seed. If keep-at can't determine a torrent's age at all, it treats that as not yet eligible rather than assuming it's fine.

## Network-wide stats

While scanning, keep-at briefly joins the swarm of every candidate anyone at all is seeding or leeching, and records, per keep-at peer found: its (best-effort) node identity, and whether it has the whole torrent (seeding) or not (leeching). `keep-at network-status` reports the totals. This keep-at peer count is metadata only - it does not gate which torrents keep-at selects (see "Seeding minimally-seeded torrents" above).

The network-status snapshot also stores the **p10 seeder floor** (x from the seed-scarcity gate, see "Seeding minimally-seeded torrents" above), recomputed from every completed scan's catalog observations so the next scan anchors its selection to the catalog's measured health.

This is necessarily an estimate, not a census:

* **Node count** is distinct IP addresses seen claiming to be keep-at, only across torrents scanned this run. A node sharing a NAT with another keep-at instance undercounts; comparing node counts across separate scans isn't a reliable trend line, since each scan only sees what it happened to probe.
* **Seeding/leeching byte totals** sum a torrent's full size once per keep-at node observed holding it complete or incomplete, deliberately not deduplicated across nodes - the point is total keep-at-attributable capacity in use, not unique data volume.
* A hostile peer could claim to be keep-at when it isn't; nothing about the extended handshake is authenticated.

Progress reporting (`processed/total candidates`) is based on how many catalog entries keep-at intends to walk through this scan (everything not already held and not keyword-blocked), computed before any network calls, so the denominator is stable even though which of those turn out to be age-eligible or scrapeable isn't known until each one is actually processed.

## Running against the real catalog

Testing against the entire live Academic Torrents catalog (rather than a hand-picked handful) surfaced problems that only show up at that scale. What follows is what broke and what changed.

### A library bug that took the whole process down

anacrolix/torrent v1.61.0 introduced a rewritten tracker-announce dispatcher (`client-tracker-announcer.go`) with an internal locking bug: under enough concurrent torrent churn, an assertion (`panicif.False`) inside it fails, which manifests as `fatal error: sync: Unlock of unlocked RWMutex` - a Go runtime fatal error, not a normal panic, so it can't be recovered from and kills the whole process outright. It reproduced consistently during a real full-catalog scan, triggered from more than one code path (adding a torrent, dropping one, and even a torrent's own periodic re-announce timer), which meant no single call site could be wrapped around to avoid it. The same release also had a webseed desync panic ([anacrolix/torrent #1036](https://github.com/anacrolix/torrent/issues/1036)) that killed the process from a background timer goroutine.

Both bugs are now patched in keep-at's fork, `github.com/tweedge/anacrolix-torrent` at tag `v1.61.0-patch2`, via a `replace` directive. The dispatcher fix makes `singleAnnounce` always re-acquire the client lock on exit (even on panic, so the caller's deferred `unlock()` never fires on an unlocked mutex), guards against a missing tracker client, recovers panics on both the announce and timer goroutines, and demotes the dispatcher's churn-induced desync assertions to warnings. The webseed fix demotes the request-view desync check to a warning (`updateWebseedRequests`). The same fork also carries the download rate-limiter fix (stale `ReserveN` timestamps let sustained throughput exceed the configured rate under concurrent connections). `v1.61.0-patch2` additionally demotes every remaining dispatcher `panicif` consistency assertion - the `trackerAnnounceHead`/`announceIndex` length checks, infohash-concurrency reconciliation, the per-tracker concurrency limit, and `initTrackerClient`'s URL sanity checks - to warnings, since all could fire under churn and abort announces. Revisit once upstream ships a version that fixes the dispatcher and keeps the rate limiter working.

### Why probing uses a second, disposable torrent client

Independent of the above, probing a candidate's swarm (see "Seeding minimally-seeded torrents") works by adding a torrent just to inspect its peers - and earlier versions of this code dropped it again immediately afterward. Rapid add-then-drop across thousands of candidates per scan is exactly the kind of churn that triggers bugs like the one above, library version notwithstanding.

So probing now happens on a dedicated `*torrent.Client` that:

* Is never used for anything else - real downloads (`AddCandidate`, resuming held torrents on startup) always go through the main client.
* Has DHT disabled. DHT's own per-torrent announce goroutine was one of the two concrete triggers found for the v1.61.0 dispatcher bug (now patched in the fork), and DHT isn't needed to answer "who else is in this swarm right now" - regular trackers are enough for that.
* Never has individual torrents dropped from it. Probed torrents just accumulate - not attempting any real transfer - and the entire client is periodically closed and replaced instead (see below), which releases everything through a code path that doesn't share the add/drop race.

DHT stays enabled on the main client: disabling it globally was tried first and measurably hurt real download connectivity (one torrent went from a 15-second download to not finishing within 90 seconds with DHT off). Isolating the churn to a disposable, DHT-free client keeps the crash risk contained without that cost.

### Bounding probe memory mid-scan

Letting probed torrents accumulate for an entire scan (rather than the previous approach: only at the start of the *next* scan) turned out not to be harmless after all, at real Academic Torrents scale. Some datasets are large enough that a torrent's piece-level bookkeeping - hashes and per-piece state, sized by piece *count*, not by how much of it keep-at has actually downloaded (nothing, for a probe) - runs into tens of megabytes on its own. In a real full-catalog run, process memory grew roughly linearly with candidates processed and was on track to exceed available RAM before the scan of ~2,850 items would have finished.

`resetProbeClient` now also runs mid-scan, every `probeClientResetInterval` (250) candidates processed, not just once between scans. This bounds peak memory to roughly what 250 candidates' worth of probes need instead of the whole catalog's. The cost is that a probe still in flight against the client being reset can error out; that's already handled the same as any other probe failure - logged, and treated as zero peers observed for that one candidate, not fatal.

### Evaluating candidates concurrently

Probing waits several seconds per candidate for peer connections to establish. Run sequentially across a catalog with hundreds or thousands of available candidates, that alone would take hours. `evaluateCandidates` now evaluates up to `evaluateConcurrency` (16) candidates at once; the actual requests to Academic Torrents (fetching `.torrent` files, scraping trackers) stay correctly rate-limited regardless, since they share one `rate.Limiter` across every goroutine.

### Keeping automatic tracker announces inside the same rate limit

`scrapeSwarm` and `attorrent.Fetcher` are careful to rate-limit keep-at's *own* requests to Academic Torrents. What isn't obvious: the underlying torrent client re-announces to every tracker in a torrent's spec on its own schedule, entirely outside that code - and in a real full-catalog run, that automatic traffic to `academictorrents.com`'s tracker was enough on its own to get keep-at rate-limited (HTTP 429) by AT. `ClientConfig.TrackerDialContext` is set to a dialer that routes connections to `academictorrents.com` through the same limiter (see `rateLimitedTrackerDialer`), so every path to AT's tracker - explicit scrapes and the library's automatic announces alike - shares one budget. Third-party trackers pulled from `.torrent` files aren't rate-limited this way, since they aren't Academic Torrents' infrastructure to protect.

### A decade of catalog entries means a lot of dead trackers

Many `.torrent` files reference third-party trackers that are now offline, blocking automated clients, or returning garbage - and webseed URLs that don't match what this version of anacrolix/torrent expects. Both are normal, not a keep-at problem, and logging every instance of them at full-catalog scale is overwhelming: one candidate can generate a dozen `"announce failed"` or `"webseed request error"` lines across its various dead trackers and webseeds.

The library's internal chatter is now suppressed by setting `ClientConfig.Logger` to a filter-level-capped logger (`analog.Default.WithFilterLevel(analog.Error)`), which drops its warnings entirely rather than passing them through. This was not the first approach tried: wrapping `ClientConfig.Slogger` with a custom `slog.Handler` that selectively dropped specific messages looked like it should work, but empirically didn't - `"announce failed"` and `"webseed request error"` never reached the wrapped handler, for reasons in the library's `Logger`-to-`Slogger` bridging that weren't worth chasing further once a reliable alternative was found. The tradeoff of the level-based approach is coarser: it silences *all* of the library's internal warnings, not just the specific noisy ones, including a same-severity warning against Academic Torrents' own tracker specifically if one ever occurs. keep-at's own logging (scan progress, its own request failures like a failed `.torrent` download or scrape) is unaffected either way, since that's separate, application-level logging that never went through the torrent client's logger.

### Reusing UDP tracker connections

`scrapeSwarm` scrapes third-party UDP trackers (BEP 15) for candidates whose primary tracker doesn't answer, and a full-catalog scrape does this thousands of times over several hours. The first implementation created a fresh `tracker.Client` (and the UDP socket it opens) for every single scrape call and closed it again immediately after. In a real full-catalog run, this started failing with "address already in use" on `listen udp :0` late into a multi-hour scan - a slow resource leak (sockets not being released fast enough relative to the rate new ones were being opened) that only became visible after thousands of create/close cycles, not in any test smaller than the real thing.

`attorrent.UDPScraper` now caches one `tracker.Client` per distinct tracker URL and reuses it for every scrape against that tracker for the lifetime of the Engine. This is also how the protocol is meant to be used, not just a workaround: BEP 15 scrapes carry a random transaction ID, and the client dispatches responses back to the matching in-flight caller by that ID under its own lock, so concurrent scrapes against the same shared client from multiple goroutines are safe.

### Disabling webseeds entirely

A real full-catalog run crashed with a nil pointer dereference deep inside anacrolix/torrent's webseed request machinery (`webseed.(*Client).StartNewRequest`), triggered from a periodic background timer - a goroutine keep-at's own code never touches, so nothing in keep-at could `recover()` from it (unlike the `safely()`-wrapped per-candidate work elsewhere). keep-at doesn't rely on webseeds in the first place (see "Storage" below and the log-noise section above) - real peers are what it actually seeds to and downloads from - so `ClientConfig.DisableWebseeds` is now set on both torrent clients. That flag stops the library from ever constructing a webseed peer for any torrent, which removes the crashing code path structurally rather than just reducing how often it's hit.

### Reporting progress during a long scrape

A full-catalog scrape can run for a long time (see above), and a log that goes quiet for that long looks the same whether keep-at is working or stuck. Three log lines mark the phase:

* `"starting scrape"`, once, right before evaluation begins - stating explicitly that it can take a while, and that downloads start gradually as the highest-priority candidates are found rather than waiting for the whole scrape to finish (see "Scans act incrementally" below).
* `"scrape in progress"`, every `progressLogInterval` (5 minutes) while it runs, with percent complete and an ETA.
* `"scrape complete, updating what keep-at holds"`, once, when it finishes, right before acting on the results.

The ETA is a straight-line extrapolation - elapsed time divided by candidates processed so far, multiplied by candidates remaining - not a measured prediction. It assumes the rest of the catalog behaves like what's already been seen, which mostly holds since Academic Torrents rate limiting dominates the per-candidate cost fairly evenly, but it's a rough guide, not a countdown to trust precisely.

### Scans act incrementally, and re-scans are cheap

keep-at used to evaluate the entire pending candidate list before acting on anything, leaving free disk idle for hours on a first full-catalog scan. That changed: candidates now stream out of evaluation as they complete, and `ScanOnce` acts on the highest-priority ones immediately - the top of the running ranking gets seeded as soon as it is known, not after the whole catalog is walked. Three changes made this both correct and fast:

- **Scrape results are cached across scans.** `scrapeSwarm` remembers each torrent's seeder/leecher counts in `scrape-cache.json` (TTL = the scan interval). A weekly re-scan reuses last week's counts instead of re-querying Academic Torrents' tracker for every catalog item, so repeat scans cost almost nothing beyond the per-candidate swarm probe.

- **Swarm probing moved to decision time.** The probe (which waits several seconds per candidate to count keep-at peers for network-status) used to run for every available candidate during evaluation. It now runs only for the candidates keep-at is actually about to act on, so probe time drops from "every available candidate" to "everything we decide to seed."

- **Acting is windowed by the torrent cap.** Each batch, keep-at ranks everything evaluated so far and acts only on the top `min(maxTorrents, evaluated)` candidates. Because `maxTorrents` is the most torrents keep-at can hold (see the RAM section), this guarantees it never seeds something that is not genuinely among the best it could hold, while still filling the best slots early. Lower-priority candidates evaluated later only get acted on if they earn a place in that top window.

- **Evaluated candidates stay lightweight.** The incremental model keeps every evaluated candidate around for the whole scan (ranking re-considers the running top window each batch). What it keeps is deliberately *small*: just title, infohash, size, and scraped swarm counts. The full parsed `.torrent` metadata - whose piece-hash arrays scale with the library's total size, not its torrent count - is written to `torrent-cache/` during evaluation and re-read from disk only when keep-at actually acts on a candidate. A full-catalog scan's memory footprint is therefore proportional to the number of candidates, not the size of the library, which is what keeps keep-at usable on a 1 GB-RAM device. To keep ranking work proportional too, acting happens once per `evaluateConcurrency` arrivals rather than once per candidate (plus a final flush), so the per-arrival re-rank is bounded instead of O(N² log N) across the whole scan.

The first scan still takes a while (it must fetch and scrape the catalog once, and probes the torrents it actually chooses), but configured storage stops sitting idle: the most urgent torrents start seeding within minutes, not after the full walk.

### Verified end to end

After the fixes above, a real run against the entire live catalog (2,850 items, 10GB cap in `/tmp`) completed its first full scan in 4h19m with zero crashes and bounded memory (peaked around 5GB, not the tens of gigabytes it was on track for before the mid-scan probe reset), selected 538 torrents to hold, and stayed stable for another 2+ hours of real downloading and seeding afterward. Every fix above was found and confirmed this way - none of them reproduced in anything smaller than the real catalog at real duration.

### The smoke test standard

That "small runs won't reproduce scale bugs" lesson is exactly why keep-at has two levels of live smoke test, both skipped unless explicitly opted in:

- **`KEEPAT_SMOKE_TEST=1`** - the fast one. Two hand-picked, verified-seeded torrents through the whole pipeline (catalog -> `.torrent` fetch -> scrape -> probe -> download -> store) in under a minute. Good for a quick "is the plumbing connected" check, but too small to exercise anything scale-dependent.

- **`KEEPAT_SMOKE_SUBSET=1`** - the real one, aimed at fitting in ~10 minutes. It fetches the live catalog, takes the smallest ~100 entries by size (the most likely to still be seeded), runs a real scan out of `/tmp`, and **asserts structural invariants rather than "didn't crash"**:

  - every candidate that survived evaluation actually issued a tracker scrape (`scrape_requests >= eligible`) - this is what catches a change like batching scrapes into multi-hash requests, which AT's tracker silently doesn't support;
  - scrape failures stay under half of processed candidates;
  - the scan actually finishes within the time budget (`processed == catalog size`);
  - at least one held torrent completes a real download with data on disk.

  Run it with `KEEPAT_SMOKE_SUBSET=1 go test ./internal/engine/ -run TestSmokeRealCatalogSubset -timeout 15m -v`. `KEEPAT_SMOKE_SIZE` and `KEEPAT_SMOKE_RATE` tune the catalog count and requests/second to AT. On a normal connection the 100-item scan completes in roughly five minutes.

The rule of thumb for new behavior that could interact with the catalog at scale: add it to the subset test's invariants, not just the two-item test. The batched-scrape regression was shipped because the two-item test looked like enough - it wasn't.

## Storage

keep-at stores each verified piece as its own gzip-compressed file, keyed by piece index under a directory named after the torrent's infohash. There's no attempt to reconstruct the original file layout on disk - keep-at prioritized conflict-free, efficient local storage, rather than being locally readable. Giving up on that constraint makes per-piece compression simple. Deleting a torrent just removes its directory and pieces.

Because pieces are gzip-compressed, space accounting is **post-compression**: the limit on a storage location is compared against actual on-disk bytes, not the nominal torrent sizes. A 100 GB torrent that stores as 60 GB only consumes 60 GB of the limit, and the 40 GB of compression gains count toward free space. This compounds on the library side too - Academic Torrents is full of textual and structured data that compresses well, so a location typically fits noticeably more than its nominal-size accounting would suggest.

**Location placement** (`chooseLocation`) and **swap math** (`selectDisplaceable`) follow the same rule. A candidate's on-disk footprint is estimated by applying the location's observed compression ratio (on-disk bytes / nominal bytes across held torrents) to its nominal size, and a displaced torrent's freed space is its actual on-disk bytes - so compression gains help fit more torrents in every path, not just the headline free-space number. On a fresh location with no held torrents, the ratio starts at 1.0 (no compression assumed) until there's evidence. Free space is additionally capped by what the device actually has free (statfs), so filesystem block slack and metadata can never let accounting drift past real capacity.

**`limit: all`** accepts the literal `all` in place of a byte count (e.g. `limit: all` in a config file, or `--storage-limit all`). It resolves at startup to `config.AllLimitFraction` (97.5%) of the device's total *formatted* capacity, measured with statfs on the location path. The fraction is deliberately below 100%: filesystems reserve blocks for their own health (ext4 defaults to reserving 5% for root), journals and metadata need room, and block-slack on many small piece files costs real space beyond their byte sum - and keep-at, running as root, can otherwise fill the last block. 97.5% is right for a dedicated drive; **it is not right for an OS drive**. On a busy system disk, keep-at filling to 97.5% leaves the OS too little headroom and can choke it for resources - hence the warning in the README to only use `all` on dedicated data drives.

To download torrents you *personally want* from AT - use your torrent client normally.

Cross-torrent deduplication (storing identical pieces once even if they appear in multiple torrents) was considered and deliberately left out. Exact piece-level duplicates across unrelated academic datasets are rare enough that the added complexity (a content-addressable store with reference counting and garbage collection) wasn't worth it for the space it'd actually save. Compression alone still helps for the occasional torrent which is textual data.

## Stalled downloads and slot hygiene

A held torrent that falls to zero seeders and never completes is pure dead weight: it occupies a RAM slot (keep-at's per-torrent memory budget is what bounds the held count), consumes disk accounting, and can never finish - with no seeders, there's no one to serve the missing pieces. And because swaps only evict *well-seeded* held torrents (see the seed margin), a 0-seeder torrent is effectively immune to being swapped out on its own. Without intervention, it would sit forever.

keep-at therefore tracks download progress per held torrent: how many pieces are fully stored, and when that count last grew (persisted in state as `completed_pieces` and `last_progress_at`). Every scan refreshes both. A torrent that still has zero seeders **and** hasn't gained a single new piece since its stall clock started - for longer than `scan.stall_eviction_timeout` (default two weeks, configurable, `0` disables) - is removed to free its slot. The clock starts at first observation and resets whenever a piece completes, so a torrent gets a full quiet window before any eviction, and a slow-but-alive download (pieces arriving from leechers' combined data even at zero seeders) is never misclassified as stalled. Torrents still listed in the Academic Torrents catalog are the only candidates; one removed from the catalog is handled by the deleted-torrent path instead.

## Todos

* **macOS and Windows service management.** The binary runs fine on both; `keep-at service install` doesn't (systemd/Linux only).
* **Peer-map availability.** See "Reasoning quickly about availability" above.
* **Authenticated node identity.** network-status trusts the BitTorrent extended handshake's claimed client name at face value.
* **Incremental action during a scan.** Done - see "Scans act incrementally, and re-scans are cheap" above.
* **Pinned anacrolix/torrent version.** keep-at is on v1.61.0 via a `replace` to `tweedge/anacrolix-torrent` (which carries backported fixes for the webseed desync panic and the tracker-announce dispatcher crash - see "A library bug that took the whole process down"). Revisit once upstream fixes the dispatcher.
