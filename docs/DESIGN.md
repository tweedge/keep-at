# Design

This is the authoritative explanation of *why* keep-at behaves the way it does: the selection math, the deliberate simplifications, and what's still missing. The README covers how to run it; this covers how it thinks.

## Deciding what to seed

Every scan, keep-at pulls Academic Torrents' catalog (`database.xml`), filters out anything keyword-blocked or too young (see "Age, precisely" below), and checks the remaining candidates' tracker scrape data. A torrent needs at least one live seed to be considered "available" - keep-at won't start a download that can never finish.

Among available candidates, fewer seeds means higher priority. If keep-at has free space, it fills it with the highest-priority candidate it can. If space is full, it'll displace currently-held torrents for a new candidate, but only if the candidate has at least `min_seed_margin` (default 3) fewer seeds than everything it would replace.

## Avoiding herd mentality

Before committing to a download, keep-at checks how many other keep-at nodes are already in that torrent's swarm (it identifies itself in the BitTorrent extended handshake, and looks for peers reporting the same string). This exists to stop every keep-at node globally from swapping to the same under-seeded torrent at once and just moving the problem somewhere else. The chance keep-at proceeds is:

```
n = aggressiveness ^ (keep-at peers already in the swarm)
```

With zero other keep-at nodes present, n is 1: go ahead confidently. As more keep-at nodes join, n shrinks toward zero (aggressiveness defaults to 0.6, and is always between 0 and 1), so later nodes progressively back off. keep-at rolls a random float and proceeds only if the roll is below n.

This same anti-cascade check runs whether keep-at is filling free space or displacing something, so it also naturally discourages every keep-at node from racing to grab the same freshly-added torrent.

One correction worth documenting: an earlier draft of this design described rolling *above* n to proceed. That's backwards from what n is defined to mean ("the chance keep-at swaps") and produces the opposite of the intended effect - a shrinking n would make later nodes pile on *more* eagerly, not less. keep-at implements the definition, not the inverted comparison: `selector.EvaluateSwap` swaps when `roll < n`.

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

While scanning, keep-at briefly joins the swarm of every candidate anyone at all is seeding or leeching - the same probe that counts keep-at peers for the anti-cascade check - and records, per keep-at peer found: its (best-effort) node identity, and whether it has the whole torrent (seeding) or not (leeching). `keep-at network-status` reports the totals.

This is necessarily an estimate, not a census:

* **Node count** is distinct IP addresses seen claiming to be keep-at, only across torrents scanned this run. A node sharing a NAT with another keep-at instance undercounts; comparing node counts across separate scans isn't a reliable trend line, since each scan only sees what it happened to probe.
* **Seeding/leeching byte totals** sum a torrent's full size once per keep-at node observed holding it complete or incomplete, deliberately not deduplicated across nodes - the point is total keep-at-attributable capacity in use, not unique data volume.
* A hostile peer could claim to be keep-at when it isn't; nothing about the extended handshake is authenticated.

Progress reporting (`processed/total candidates`) is based on how many catalog entries keep-at intends to walk through this scan (everything not already held and not keyword-blocked), computed before any network calls, so the denominator is stable even though which of those turn out to be age-eligible or scrapeable isn't known until each one is actually processed.

## Running against the real catalog

Testing against the entire live Academic Torrents catalog (rather than a hand-picked handful) surfaced problems that only show up at that scale. What follows is what broke and what changed.

### A library bug that took the whole process down

anacrolix/torrent v1.61.0 introduced a rewritten tracker-announce dispatcher (`client-tracker-announcer.go`) with an internal locking bug: under enough concurrent torrent churn, an assertion (`panicif.False`) inside it fails, which manifests as `fatal error: sync: Unlock of unlocked RWMutex` - a Go runtime fatal error, not a normal panic, so it can't be recovered from and kills the whole process outright. It reproduced consistently during a real full-catalog scan, triggered from more than one code path (adding a torrent, dropping one, and even a torrent's own periodic re-announce timer), which meant no single call site could be wrapped around to avoid it.

keep-at pins `github.com/anacrolix/torrent` to **v1.60.0**, the release immediately before this dispatcher was introduced, which doesn't have the bug. This is a real dependency downgrade, not a config toggle - revisit it once a fixed version exists upstream.

### Why probing uses a second, disposable torrent client

Independent of the above, probing a candidate's swarm (see "Avoiding herd mentality") works by adding a torrent just to inspect its peers - and earlier versions of this code dropped it again immediately afterward. Rapid add-then-drop across thousands of candidates per scan is exactly the kind of churn that triggers bugs like the one above, library version notwithstanding.

So probing now happens on a dedicated `*torrent.Client` that:

* Is never used for anything else - real downloads (`AddCandidate`, resuming held torrents on startup) always go through the main client.
* Has DHT disabled. DHT's own per-torrent announce goroutine was one of the two concrete triggers found for the v1.61.0 bug, and DHT isn't needed to answer "who else is in this swarm right now" - regular trackers are enough for that.
* Never has individual torrents dropped from it. Probed torrents just accumulate for the rest of the current scan (harmless - they hold no real data and attempt no transfer) and the entire client is closed and replaced at the start of the next scan, which releases everything through a code path that doesn't share the add/drop race.

DHT stays enabled on the main client: disabling it globally was tried first and measurably hurt real download connectivity (one torrent went from a 15-second download to not finishing within 90 seconds with DHT off). Isolating the churn to a disposable, DHT-free client keeps the crash risk contained without that cost.

### Evaluating candidates concurrently

Probing waits several seconds per candidate for peer connections to establish. Run sequentially across a catalog with hundreds or thousands of available candidates, that alone would take hours. `evaluateCandidates` now evaluates up to `evaluateConcurrency` (16) candidates at once; the actual requests to Academic Torrents (fetching `.torrent` files, scraping trackers) stay correctly rate-limited regardless, since they share one `rate.Limiter` across every goroutine.

### Keeping automatic tracker announces inside the same rate limit

`scrapeSwarm` and `attorrent.Fetcher` are careful to rate-limit keep-at's *own* requests to Academic Torrents. What isn't obvious: the underlying torrent client re-announces to every tracker in a torrent's spec on its own schedule, entirely outside that code - and in a real full-catalog run, that automatic traffic to `academictorrents.com`'s tracker was enough on its own to get keep-at rate-limited (HTTP 429) by AT. `ClientConfig.TrackerDialContext` is set to a dialer that routes connections to `academictorrents.com` through the same limiter (see `rateLimitedTrackerDialer`), so every path to AT's tracker - explicit scrapes and the library's automatic announces alike - shares one budget. Third-party trackers pulled from `.torrent` files aren't rate-limited this way, since they aren't Academic Torrents' infrastructure to protect.

### A decade of catalog entries means a lot of dead trackers

Many `.torrent` files reference third-party trackers that are now offline, blocking automated clients, or returning garbage - and webseed URLs that don't match what this version of anacrolix/torrent expects. Both are normal, not a keep-at problem, and logging every instance of them at full-catalog scale is overwhelming: one candidate can generate a dozen `"announce failed"` or `"webseed request error"` lines across its various dead trackers and webseeds.

The library's internal chatter is now suppressed by setting `ClientConfig.Logger` to a filter-level-capped logger (`analog.Default.WithFilterLevel(analog.Error)`), which drops its warnings entirely rather than passing them through. This was not the first approach tried: wrapping `ClientConfig.Slogger` with a custom `slog.Handler` that selectively dropped specific messages looked like it should work, but empirically didn't - `"announce failed"` and `"webseed request error"` never reached the wrapped handler, for reasons in the library's `Logger`-to-`Slogger` bridging that weren't worth chasing further once a reliable alternative was found. The tradeoff of the level-based approach is coarser: it silences *all* of the library's internal warnings, not just the specific noisy ones, including a same-severity warning against Academic Torrents' own tracker specifically if one ever occurs. keep-at's own logging (scan progress, its own request failures like a failed `.torrent` download or scrape) is unaffected either way, since that's separate, application-level logging that never went through the torrent client's logger.

### Reporting progress during a long scrape

A full-catalog scrape can run for a long time (see above), and a log that goes quiet for that long looks the same whether keep-at is working or stuck. Three log lines mark the phase:

* `"starting scrape"`, once, right before evaluation begins - stating explicitly that it can take a while, and that keep-at won't add, swap, or remove anything until it's done (see below).
* `"scrape in progress"`, every `progressLogInterval` (2 minutes) while it runs, with percent complete and an ETA.
* `"scrape complete, updating what keep-at holds"`, once, when it finishes, right before acting on the results.

The ETA is a straight-line extrapolation - elapsed time divided by candidates processed so far, multiplied by candidates remaining - not a measured prediction. It assumes the rest of the catalog behaves like what's already been seen, which mostly holds since Academic Torrents rate limiting dominates the per-candidate cost fairly evenly, but it's a rough guide, not a countdown to trust precisely.

### A scan doesn't act until it finishes evaluating

Because ranking a candidate (see "Deciding what to seed") depends on comparing it against every other candidate found that scan, `ScanOnce` evaluates the *entire* pending candidate list before it starts adding or swapping anything. On the real catalog (a few thousand candidates, rate-limited against Academic Torrents), a first scan can take a long time before keep-at downloads anything at all, even with free space sitting idle the whole time. This is a real, currently-unaddressed tradeoff between "always pick the single most urgent candidate across the whole catalog" and "start using free space immediately" - not a bug, but a known cost of the current design worth revisiting.

## Storage

keep-at stores each verified piece as its own gzip-compressed file, keyed by piece index under a directory named after the torrent's infohash. There's no attempt to reconstruct the original file layout on disk - keep-at prioritized conflict-free, efficient local storage, rather than being locally readable. Giving up on that constraint makes per-piece compression simple. Deleting a torrent just removes its directory and pieces.

To download torrents you *personally want* from AT - use your torrent client normally.

Cross-torrent deduplication (storing identical pieces once even if they appear in multiple torrents) was considered and deliberately left out. Exact piece-level duplicates across unrelated academic datasets are rare enough that the added complexity (a content-addressable store with reference counting and garbage collection) wasn't worth it for the space it'd actually save. Compression alone still helps for the occasional torrent which is textual data.

## Todos

* **macOS and Windows service management.** The binary runs fine on both; `keep-at service install` doesn't (systemd/Linux only).
* **Peer-map availability.** See "Reasoning quickly about availability" above.
* **Authenticated node identity.** The anti-cascade check and network-status both trust the BitTorrent extended handshake's claimed client name at face value.
* **Incremental action during a scan.** See "A scan doesn't act until it finishes evaluating" above - a first full-catalog scan can leave configured storage unused for a long time before keep-at downloads anything.
* **Pinned anacrolix/torrent version.** v1.61.0 has a crashing bug (see "Running against the real catalog"); keep-at is on v1.60.0 until a fix lands upstream.
