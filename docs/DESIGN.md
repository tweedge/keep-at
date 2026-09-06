# keep-at design notes

The decisions behind keep-at's behavior, and the reasoning for each. Written from the code as it stands - if a section disagrees with `src/`, the code wins and this file needs updating.

The central question keep-at answers is: of everything on Academic Torrents, what does *this host* hold? The answer has to balance three things at once: the network's need (poorly-seeded torrents first), the host's disk (never exceed a location's limit), and the host's RAM (piece and peer bookkeeping scales with torrent count, not bytes - see "Memory" below). Everything in this file is downstream of that triangle.

## Seeding minimally-seeded torrents

What keep-at actually does is keep all its nodes from piling onto the same torrent. So rather than treating "seeds below threshold" as a simple yes-or-no, it uses a probabilistic gate: the chance a node *acts* on a candidate with `seeders` total seeds is `n = aggressiveness ^ max(0, seeders - x)`, where both `aggressiveness` and `x` are configurable (`aggressiveness`, and x is the p10 seeder floor measured from the last completed scan - see below). A 1-seeder torrent at the default 0.6 aggressiveness is a near-certainty (provided there's at least one seed to download from at all, which the availability check enforces), while a torrent with many seeds above the floor is effectively never selected. As more keep-at nodes join a torrent, the others back off automatically - mirroring the cascade intuition that if everyone grabs the same torrent, nobody covers the next one. The roll is drawn per evaluation: every candidate in every scan gets its own independent chance, and candidates that lose the roll are skipped entirely (with no logging - see "Why failed seed-scarcity rolls don't log" below).

The catch in that higher-seed regime is proving that these torrents *aren't* kept alive by us: any one node's view of a torrent as "not having keep-at support" might just be a snapshot of the swarm at a bad moment (or swarms that are often invisible to a single observer - DHT-only or client-restricted, for example). There's no cheap way to check, and doing the slow verification per candidate per scan is impractical, so we go the simple-and-safe route: selection is keyed on the tracker's *total* seeder count, and keep-at only ever *adds* to a swarm's availability, never considering removing healthy support as a goal. That's a tractable, honest thing to compute, and it avoids degenerate strategies where keep-at nodes chase each other in and out of the same torrents.

The number of other keep-at nodes in a torrent's swarm is deliberately **not** part of this gate. It's network-status data (see "Network-wide stats" below), but it doesn't change whether keep-at selects a torrent. What matters is how healthy the torrent is overall, which only the total seeder count captures.

### Where the p10 floor comes from

x is the nearest-rank 10th percentile of seeder counts across the catalog entries the last completed scan saw with at least one seeder (`seeder_floor` in `src/selector.rs`, persisted in `network-stats.json`). Before any scan has completed, the floor is 0, which the gate clamps to behave as floor 1 - the first scan is therefore conservative, treating a 1-seeder torrent as a near-certainty and backing off fast above it. Each completed scan recomputes the floor from its own scrape data, so the gate tracks the catalog's actual health: as overall Academic Torrents health improves, the floor rises and keep-at keeps finding content to store rather than holding back on a stale assumption - and the floor only rises when health genuinely improves, never speculatively.

### Why failed seed-scarcity rolls don't log

Rejecting a candidate because its seed-scarcity roll failed is the routine, expected outcome for any well-seeded torrent - and at full-catalog scale that is most candidates. Logging every one produced hundreds of `seed-scarcity roll failed` lines per scan, drowning out the lines that actually matter (adds, swaps, margin failures, RAM-cap skips). So the decision is made silently: the roll is still drawn and still gates, it just doesn't emit. Every other outcome - added, swapped, margin failure, RAM cap, fetch/scrape errors - still logs at its usual level.

## Ranking: seeders first, size bias only within a band

Within a scan, evaluated candidates are ranked fewest-seeders-first (`rank_candidates` in `src/selector.rs`), with unavailable (zero-seed) candidates excluded outright. Seeder count is the absolute primary key: no size preference can ever promote a well-seeded torrent above a poorly-seeded one, and there is a unit test pinning exactly that (a 10 TiB 1-seeder outranks a 1 KiB 2-seeder at every bias).

The size bias only orders *within* an equal-seeder band. At startup keep-at computes a bias exponent in [-1, +1] from the host's RAM:disk ratio (`size_bias_for_ratio` in `src/engine/ram.rs`: 80%-of-RAM budget vs total configured disk limits, log-interpolated between 8 GiB/TiB → −1 and 0.25 GiB/TiB → +1, so a balanced 1 GiB:1 TiB host lands near +0.15). Positive bias ranks ties by bytes-per-RAM-byte (larger torrents first); zero or negative bias ranks smallest-first, the historical order. The exponent is small on purpose - a nudge, not a vote - so size steers which torrents fill the budget without ever overriding urgency. The bias is recomputed from static limits at each startup (limits don't change mid-run), logged as `size-bias`, and stored on the Engine for the scan's lifetime.

Why piece count is the RAM axis: rqbit's per-torrent bookkeeping scales ~32 B/piece (measured slope fit over 64..32768 pieces/torrent), so a 2 GiB 100-piece torrent costs meaningfully less RAM than a 2 GiB 100k-piece one at the same disk footprint. `torrent_ram` prices each candidate as 256 KiB base + 64 B/piece + 48 KiB per live peer at the budget's peer limit, and the bias compares those prices - never raw byte size alone.

## Memory

keep-at's memory use scales with how many torrents it holds and how many *pieces* they have - rqbit keeps per-torrent bookkeeping of ~256 KiB plus ~64 B per piece, plus per-live-peer buffers sized by the session's peer limit - not with their total byte size. The figures are measured, not estimated: paused torrents at ~126 KiB, live no-peer torrents at ~150 KiB fixed plus a ~30 B/piece slope, each live peer holding a 32 KiB socket read buffer plus task overhead (see the module docs in `src/engine/ram.rs` for the probe methodology).

Three mechanisms turn that model into behavior:

- **The RAM budget caps the torrent count** (`max_torrents_for_budget`): 80% of system RAM by default (`--max-ram` can lower it, never raise it past the 80% hard cap), divided by the typical-torrent price (~1k pieces at the budget's peer limit). A 512 MiB Pi gets ~600 slots; an 8 GiB box ~5,400. When system RAM can't be measured at all, the RAM-driven cap is disabled rather than treating zero as "hold nothing", with a warning.
- **The peer limit scales with the budget** (`peer_limit_for_budget`): 20 peers/torrent on comfortable hosts (≥4 GiB budget), stepping to 12 / 8 / 4 as the budget shrinks. Peer buffers are the dominant per-torrent term, so this is the main lever that lets a small host hold several times more torrents - at the cost of slower per-torrent swarms, the right trade for a seeder whose torrents are mostly already complete.
- **RAM headroom gates every add** (`ram_headroom_bytes`): free-space fill refuses any candidate whose priced footprint exceeds the budget minus the summed footprints of what's held (held torrents price from their stored piece counts), and swaps require the displaced set to cover the candidate's RAM cost as well as its disk. The budget is therefore a ceiling the scan can approach but never cross, regardless of free disk - which is what lets a 512 MiB node sit safely next to a terabyte of empty disk.

Recommended provisioning is **1 GiB of RAM per 1 TB of storage**; see the README's napkin math for the derivation. The startup log prints `budget`, `peer-limit`, `size-bias`, and `max-torrents` so the arithmetic is checkable per host.

## Multi-torrent swaps

A candidate can be bigger than any single held torrent. When that happens, keep-at looks within one storage location for a *set* of held torrents to displace together, not just one (`select_displaceable` in `src/engine/scan.rs`):

1. Filter to held torrents that individually clear the seed margin against the candidate (`heldSeeders - min_seed_margin >= candidateSeeders`). A torrent that wouldn't qualify on its own never gets swept in just because it's bundled with others that do.
2. Sort the qualifying torrents by the same adjusted score ranking uses, ascending - i.e. evict the worst bytes-per-RAM first on a large-biased host, the largest first on a small-biased one, the highest-seeded first at zero bias. Eviction order is the exact reverse of rank order by construction (pinned by the `eviction_score_mirrors_rank` unit test), so swaps reinforce ranking instead of fighting it.
3. Accumulate them in that order until their combined size covers the candidate's disk need *and* their combined priced RAM covers its RAM cost. If even every qualifying torrent in that location combined isn't enough of either, that location can't take the swap.

This is a greedy selection, not a minimal one - it doesn't search for the smallest possible subset that would fit, just the first prefix that does. That keeps the logic simple and predictable at the cost of occasionally evicting one more torrent than a smarter bin-packing solution would need.

## Reasoning quickly about availability (for choosing torrents)

A torrent could theoretically be "available" with zero full seeds, if enough partial peers between them happen to cover 100% of the data. keep-at doesn't attempt to detect that case: verifying it means inspecting every peer's piece map across the whole swarm, which is expensive to do for every catalog candidate on every scan. keep-at uses the cheaper, conservative signal instead - at least one live seed - and accepts that this misses the rare fragmented-but-complete swarm.

## Reasoning conservatively about age (for moderation)

Academic Torrents' `database.xml` doesn't include an upload date, and neither does their API - the closest thing, a paper's publication date, isn't the same thing and would make the moderation delay meaningless. Instead, keep-at reads the `creation date` field baked into each `.torrent` file at fetch time, which is set when the torrent was created (verified against torrents from 2013 through 2026 while building this). A torrent needs to be at least `moderation_delay` old (default 7 days) before keep-at will touch it, giving Academic Torrents' staff time to catch anything that shouldn't be there and boot it off the platform - helping ensure your keep-at node never picks up data you don't want it to seed. If keep-at can't determine a torrent's age at all, it treats that as not yet eligible rather than assuming it's fine.

## Network-wide stats

The node itself trusts the tracker: selection is keyed on total seeder counts from the tracker scrape, and keep-at never joins a candidate's swarm during its day-to-day scans (see "Seeding minimally-seeded torrents" above).

Counting other keep-at nodes is a separate, explicit, on-demand operation: `keep-at network-status` (`src/census.rs`). It walks the whole catalog and, per torrent, fetches the metadata, scrapes AT's tracker, and briefly joins the swarm with a disposable scraper-identity session to record, per keep-at peer found: its (best-effort) node identity, and whether it has the whole torrent (seeding) or not (leeching). The command prints a running status line every 25 torrents and a summary when complete. This is RAM- and time-heavy by design - it joins every torrent's swarm, and each probe can wait up to the probe timeout (10s default) for peers - so it's something an operator runs when they want the network picture, never something the node does as part of normal operation.

The census also computes the **p10 seeder floor** (x from the seed-scarcity gate, see above) fresh from its own scrapes, the same figure scans persist for the next scan's anchor.

The keep-at counts are necessarily an estimate, not a complete census:

* **Node count** is distinct IP addresses seen claiming to be keep-at, only across torrents the census probed. A node sharing a NAT with another keep-at instance undercounts; a node whose address changes between censuses overcounts across runs.
* **Seeding/leeching byte totals** sum a torrent's full size once per keep-at node observed holding it complete or incomplete, deliberately not deduplicated across nodes - the point is total keep-at-attributable capacity in use, not unique data volume.
* A hostile peer could claim to be keep-at when it isn't; nothing about the extended handshake is authenticated.

Peer identity comes from rqbit's per-peer `client_name`, which it fills from the BEP 10 extended handshake's `v` string - the same string the seeder (`keep-at/version (seeder)`) and scraper (`keep-at/version (scraper)`) identities set, so the roles stay distinguishable on the wire. Because rqbit only reports the handshake string and not each peer's piece map, per-peer completeness is a documented estimate: a keep-at peer counts as seeding when the probed torrent is complete locally, leeching otherwise (see `keep_at_peers` in `src/census.rs` for the exact caveats).

Each probe builds a fresh short-lived session (scraper identity, ephemeral listen port, DHT off, peer limit 8) and tears it down - session stop, torrent delete-with-files, scratch dir removal - immediately after, so per-torrent peak memory never accumulates beyond one probed torrent at a time.

Progress reporting (`processed/total candidates`) is based on how many catalog entries keep-at intends to walk through this scan (everything not already held and not keyword-blocked), computed before any network calls, so the denominator is stable even though which of those turn out to be age-eligible or scrapeable isn't known until each one is actually processed.

## Running against the real catalog

Testing against the entire live Academic Torrents catalog (rather than a hand-picked handful) surfaced problems that only show up at that scale. What follows is what broke and what changed.

### Keeping automatic tracker announces inside the shared rate limit

Scrapes and `.torrent` fetches go through one shared token bucket (`RateLimiter`, default 0.5 req/s to AT infrastructure), but the rqbit session re-announces to trackers on its own schedule, outside that code - and in a real full-catalog run, that automatic traffic to `academictorrents.com`'s tracker was enough on its own to draw HTTP 429s from AT. So every torrent keep-at adds gets `force_tracker_interval: 30 min` (see `add_torrent_bytes` in `src/engine/torrents.rs`): re-announces use the tracker's own minimum interval or 30 minutes, whichever is longer, and never a faster cadence. Combined with AT-only tracker filtering (below), automatic announce traffic stays minimal. Third-party trackers pulled from `.torrent` files aren't rate-limited this way, since they aren't Academic Torrents' infrastructure to protect. When AT throttles anyway, the scrape path treats 429s as transient: warn, back off 60s (shutdown-aware), fail the candidate fast without caching anything, and let the next scan retry it.

### Held torrents only announce to Academic Torrents' own tracker

The rqbit session re-announces to every tracker in a torrent's spec on its own schedule, and AT catalog entries list up to a dozen mostly-dead third-party trackers each - so every held torrent would otherwise cycle announce timeouts against dead public trackers all day. keep-at is an AT seeder, so before adding, tracker tiers are filtered to AT's own hosts (`at_trackers_only` in `src/atkey.rs`), with the per-user announce URL substituted when an API key is configured. If a torrent lists no AT tracker at all, the list passes through unchanged - zero trackers would leave no discovery. The scrape path was already AT-first; this keeps the announce path consistent with it.

### DHT on for seeding, off for probing

The main seeder session runs with DHT on: disabling it globally was tried first and measurably hurt real download connectivity (one torrent went from a 15-second download to not finishing within 90 seconds with DHT off). The census's disposable probe sessions run with DHT off - DHT isn't needed to answer "who else is in this swarm right now", regular trackers are enough for that, and it keeps each probe's footprint to the bare minimum.

### No webseeds, no UDP scrapes

rqbit has no webseed support, and keep-at doesn't need it: real peers are what it seeds to and downloads from, so there is nothing to disable - the concept simply doesn't exist in this stack. Likewise, tracker scrapes are HTTPS-only with no UDP/BEP-15 fallback; the AT tracker answers for AT content, and torrents whose trackers are all UDP-only are skipped quietly (their last error reads "unsupported tracker scheme" only where no tracker answered at all).

### Defending against malformed tracker URLs

Some AT `.torrent` files carry tracker strings with trailing bencode garbage leaked into the byte string (observed: `https://...announce.php13:announce-listll41:https://...`). The parser truncates each raw tracker span at the end of the URL (`trunc_url` in `src/attorrent.rs`): cut at the first interior `<digits>:` run whose prefix is already a valid URL (a real port like `:1337` is kept - its digit run is preceded by host characters with no `/` before it), then validate the result. Covered by the `tracker_truncation` unit test.

### Reporting progress during a long scrape

A full-catalog scrape can run for a long time (see above), and a log that goes quiet for that long looks the same whether keep-at is working or stuck. Two log lines mark the phase:

* `"starting scrape"`, once, right before evaluation begins - stating explicitly that it can take a while, and that downloads start gradually as the highest-priority candidates are found rather than waiting for the whole scrape to finish (see "Scans act incrementally" below).
* `"scrape complete"`, once, when it finishes - with available/processed/total counts, elapsed time, library bytes, fetch/scrape/cache counters, eligible count, and the recomputed seeder floor - right before acting on the results.

(Earlier builds also logged `"scrape in progress"` every 5 minutes with percent complete and an ETA; the current code saves progress to `network-stats.json` every 2 seconds instead, which `status`-adjacent tooling reads - the periodic log line was removed to keep the log readable at full-catalog scale.)

### Scans act incrementally, and re-scans are cheap

keep-at used to evaluate the entire pending candidate list before acting on anything, leaving free disk idle for hours on a first full-catalog scan. That changed: `scan_once` evaluates the walk, then acts over its output in arrival-sized batches - the top of the running ranking gets seeded as soon as it is known, not after the whole catalog is walked. Three changes made this both correct and fast:

- **Scrape results are cached across scans.** Each torrent's seeder/leecher counts persist in `scrape-cache.json` (TTL = the scan interval). A weekly re-scan reuses last week's counts instead of re-querying Academic Torrents' tracker for every catalog item, so repeat scans cost almost nothing.
- **No swarm probing during scans.** Selection is keyed entirely on the tracker's seeder counts (see "Network-wide stats"); keep-at never joins a candidate's swarm while scanning. The network-status census that counts keep-at peers is a separate, operator-invoked command.
- **Acting is windowed by the torrent cap.** Each batch, keep-at ranks everything evaluated so far and acts only on the top `min(max_torrents, evaluated)` candidates. Because `max_torrents` is the most torrents keep-at can hold (see the RAM section), this guarantees it never seeds something that is not genuinely among the best it could hold, while still filling the best slots early. Lower-priority candidates evaluated later only get acted on if they earn a place in that top window.

- **Evaluated candidates stay lightweight.** The incremental model keeps every evaluated candidate around for the whole scan (ranking re-considers the running top window each batch). What it keeps is deliberately *small*: just title, infohash, size, piece count, and scraped swarm counts. The full parsed `.torrent` metadata is written to `torrent-cache/` during evaluation and re-read from disk only when keep-at actually acts on a candidate (add, swap, resume, and probe paths all re-read bytes from the cache file at use time). A full-catalog scan's memory footprint is therefore proportional to the number of candidates, not the size of the library, which is what keeps keep-at usable on a 1 GB-RAM device. To keep ranking work proportional too, acting happens once per `EVALUATE_CONCURRENCY` (16) arrivals rather than once per candidate (plus a final flush), so the per-arrival re-rank is bounded instead of O(N² log N) across the whole scan.

The first scan still takes a while (it must fetch and scrape the catalog once), but configured storage stops sitting idle: the most urgent torrents start seeding within minutes, not after the full walk.

**The catalog is walked in shuffled order.** Academic Torrents' `database.xml` groups torrents by upload/series, so giant datasets cluster into contiguous runs - hundreds of datasets over 100 GB, e.g. the whole noaa-ncei block. Walking it in that order made the 16 concurrent evaluation workers all fetch and parse multi-megabyte `.torrent` files (tens of thousands of piece hashes each) at once, spiking CPU and RAM together and stalling the scrape on exactly those segments (observed on a live node: 228 of 2816 candidates in 47 minutes, stuck at the noaa-ncei cluster). `scan_once_shutdown` now shuffles the catalog's items before evaluation, spreading the giants across the whole scan so only a few are in flight at any moment. It's a pure permutation - every candidate is still evaluated and ranked exactly once - and the incremental acting described above still fills the top slots early, just no longer biased toward whatever cluster happened to come first in the XML.

**Oversized torrents are disqualified before any work is done on them.** A torrent bigger than every storage location's *total* capacity can never be stored on the host, no matter what keep-at displaces - so fetching its metadata, scraping its trackers, and evaluating it are pure waste (the giant noaa-ncei datasets, up to 17TB against a 10GB cap, were exactly that). `scan_once_shutdown` computes `max_fittable_size` (the largest single location's capacity) before the scrape begins, counts how many candidates exceed it, logs that disqualification at scrape start, and evaluation skips them entirely - they're never fetched or scraped.

**Scans shut down promptly.** Long phases (held refresh, evaluation dispatch, acting) check a shutdown watch channel between units of work, and the evaluation drain aborts dispatch rather than awaiting wedged workers - abandoned tasks finish in the background while the scan bails. The binary additionally wraps the whole run in a 20s post-signal deadline plus a 5s bounded session close (mirroring the systemd unit's `TimeoutStopSec=30`), so SIGTERM always exits promptly even if a scrape wedges mid-flight. State on disk is already consistent (atomic writes everywhere), so an abort never corrupts anything - the next scan simply resumes from persisted state.

### Verified end to end

After the fixes above, a real run against the entire live catalog (~2,850 items, 5 GB cap in `/tmp`) completed its first full scan in about 2 hours, selected and held 200+ torrents, and stayed stable through rescans with real downloading and seeding throughout - zero crashes, RSS steady in the low hundreds of MB on a release build. Every fix above was found and confirmed this way - none of them reproduced in anything smaller than the real catalog at real duration.

### The smoke test standard

That "small runs won't reproduce scale bugs" lesson is exactly why keep-at has two levels of live smoke test, both skipped unless explicitly opted in:

- **`KEEPAT_SMOKE_TEST=1`** - the fast one. Two hand-picked, verified-seeded torrents through the whole pipeline (catalog -> `.torrent` fetch -> scrape -> download -> store) in under a minute, driving a real `Engine::scan_once` against a locally served catalog. Good for a quick "is the plumbing connected" check, but too small to exercise anything scale-dependent.
- **`KEEPAT_SMOKE_SUBSET=1`** - the real one, aimed at fitting in ~10 minutes. It fetches the live catalog, takes the smallest ~100 entries by size (the most likely to still be seeded), serves them locally, runs a real engine scan out of `/tmp`, and **asserts structural invariants rather than "didn't crash"**:

  - the scan actually finishes within the time budget (`processed == total`);
  - every eligible candidate actually issued a tracker scrape (`scrape_requests >= eligible`) - this is what catches a change like batching scrapes into multi-hash requests, which AT's tracker silently doesn't support;
  - scrape failures stay under half of processed candidates;
  - at least one held torrent completes a real download with data on disk.

  Run it with `KEEPAT_SMOKE_SUBSET=1 cargo test --test smoke` (subset test, ~10 min). `KEEPAT_SMOKE_SIZE` and `KEEPAT_SMOKE_RATE` tune the catalog count and requests/second to AT.

The rule of thumb for new behavior that could interact with the catalog at scale: add it to the subset test's invariants, not just the two-item test.

## Storage

keep-at stores each torrent's real files under `<location>/<infohash-hex>/` as plain sparse files - directly usable, never overlapping between torrents in one location. Deleting a torrent removes its directory. Torrents are stored as-is (no compression, no reconstructed-layout tricks): what the swarm serves is what's on disk.

Because files land sparsely, space accounting is **nominal-plus-buffer**: the limit on a storage location is compared against each held torrent's nominal size plus a small fixed per-torrent buffer (256 KiB, covering the cached `.torrent` file and state overhead), not actual on-disk bytes - so a partially-downloaded torrent already reserves its full eventual footprint and keep-at can never over-commit a location by filling it with half-finished downloads. Free space is additionally capped by what the device actually reports free (statvfs `f_bavail`), so filesystem block slack and metadata can never let accounting drift past real capacity. `hosted-torrents` is the one view that reports real on-disk bytes, since that's the operator asking "what's actually there".

**Location placement** (`choose_location`) fills locations proportionally to free space with probability proportional to each location's free bytes (weighted random among locations that fit the candidate), so multiple drives fill roughly evenly over time instead of one disk taking everything until it's full. See CONFIG.md for the flag/file forms.

**`limit: all`** accepts the literal `all` in place of a byte count (e.g. `limit: all` in a config file, or `--storage-limit all`). It resolves at startup to 97.5% (`ALL_LIMIT_FRACTION`) of the device's total *formatted* capacity, measured with statvfs on the location path (via a minimal hand-rolled binding, avoiding a libc dependency). The fraction is deliberately below 100%: filesystems reserve blocks for their own health (ext4 defaults to reserving 5% for root), journals and metadata need room - and keep-at can otherwise fill the last block. 97.5% is right for a dedicated drive; **it is not right for an OS drive**. On a busy system disk, keep-at filling to 97.5% leaves the OS too little headroom and can choke it for resources - hence the warning in the README to only use `all` on dedicated data drives.

To download torrents you *personally want* from AT - use your torrent client normally.

Cross-torrent deduplication (storing identical pieces once even if they appear in multiple torrents) was considered and deliberately left out. Exact piece-level duplicates across unrelated academic datasets are rare enough that the added complexity (a content-addressable store with reference counting and garbage collection) wasn't worth it for the space it'd actually save.

## Stalled downloads and slot hygiene

A held torrent that falls to zero seeders and never completes is pure dead weight: it occupies a RAM slot (keep-at's per-torrent memory budget is what bounds the held count), consumes disk accounting, and can never finish - with no seeders, there's no one to serve the missing pieces. And because swaps only evict *well-seeded* held torrents (see the seed margin), a 0-seeder torrent is effectively immune to being swapped out on its own. Without intervention, it would sit forever.

keep-at therefore tracks download progress per held torrent: verified bytes on disk, and when that count last grew (persisted in state as `completed_pieces` and `last_progress_at`). Every scan refreshes both - from rqbit's `progress_bytes` when the torrent is managed, else from the on-disk dir size as a conservative proxy. A torrent that still has zero seeders **and** hasn't gained a single new byte since its stall clock started - for longer than `scan.stall_eviction_timeout` (default two weeks, configurable, `0` disables) - is removed to free its slot. The clock starts at first observation and resets whenever progress grows, so a torrent gets a full quiet window before any eviction, and a slow-but-alive download is never misclassified as stalled. Torrents still listed in the Academic Torrents catalog are the only candidates; one removed from the catalog is handled by the deleted-torrent path instead (removed unless `preserve_deleted_torrents` is set).

## Todos

* **Peer-map availability.** See "Reasoning quickly about availability" above.
* **Authenticated node identity.** network-status trusts the BitTorrent extended handshake's claimed client name at face value.
