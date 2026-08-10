# keep-at release notes

## v0.7.5-beta - smooth and speed up scans for RAM-limited hosts

This is a beta release for field validation of the scan changes below; the next stable cut will be identical apart from the version tag.

### Oversized torrents are disqualified before any work is done on them

A torrent larger than every storage location's total capacity can never be stored on the host, no matter how well it compresses or what keep-at displaces. `keep-at network-status` now reports how many catalog candidates are disqualified on that basis, `keep-at` logs it at scrape start (`disqualified oversized candidates before scrape`), and they are skipped before any metadata is fetched, any tracker is scraped, or any swarm is probed. On the host this was developed against, that means the giant noaa-ncei datasets (up to 17TB against a 10GB cap) no longer stall the scrape at all.

### The catalog is walked in shuffled order

Academic Torrents' `database.xml` groups datasets by upload/series, so giant torrents cluster into contiguous runs - hundreds of datasets over 100GB, e.g. the whole noaa-ncei block. Walking the catalog in that order meant all 16 concurrent evaluators were parsing multi-megabyte `.torrent` files at once, stalling the scrape on exactly those segments (observed: 228 of 2816 candidates in 47 minutes, stuck at the noaa-ncei cluster). The walk order is now shuffled, spreading the giants across the whole scan so only a few are in flight at any moment. Every candidate is still evaluated exactly once.

### Scan memory is returned to the OS, not left at its peak

A full-catalog scan parses gigabytes of `.torrent` metadata and the probe client accumulates per-piece bookkeeping for every probed candidate. Go's GC reclaims that heap, but the runtime returns freed memory to the OS lazily, so RSS could sit at the scan's peak long after the work finished - 7GB resident on a host whose held torrents account for ~1-2GB. `keep-at` now forces the reclaimed memory back to the OS after each mid-scan probe-client reset and at scan completion, and drops the accumulated probe client after the scan so its bookkeeping isn't carried into the idle period.

### Slow startup is no longer silent

Resuming held torrents can take a long time when a lot of data is held (the client walks every stored piece to determine completion state - ~17 minutes for 1.6TB across 88 torrents on a slow host, versus well under a second for a handful of small ones). `keep-at` now logs an immediate `resuming held torrents` line, then one `resume in progress` line every 5 minutes naming what it's currently working on and how far through it is, so a slow start isn't a black box.

## v0.7.4-beta - anchor selection to the catalog's actual health, and quiet the log

This is a beta release for field validation of the selection change below; the next stable cut will be identical apart from the version tag.

### Selection is now anchored to the catalog's p10 seeder floor

The seed-scarcity gate previously compared each candidate against a fixed baseline of one seeder: `n = aggressiveness ^ (seeders - 1)`. The baseline now comes from the catalog itself. The chance keep-at proceeds with a candidate is:

```
n = aggressiveness ^ max(0, seeders - x)
```

where x is the **p10 seeder floor**: the 10th percentile of seeder counts across all catalog torrents the last completed scan saw with at least one seeder, stored in the network-status snapshot and reported by `keep-at network-status`.

The first scan still behaves exactly as before - with no completed scan there's no measured floor, so x is treated as 1 and the gate reduces to the original `aggressiveness ^ (seeders - 1)`, keeping the initial pass conservative. After each completed scan the floor is recomputed from what that scan observed.

This makes keep-at respond to the network's real health instead of assuming it:

- As overall Academic Torrents health improves, the floor rises with it, and keep-at nodes with space available keep finding content to store to fill that space - they don't hold back just because the catalog got healthier.
- The floor only rises when health genuinely improves. If it doesn't improve above the measured floor, keep-at stays just as effective at smaller scale - it never assumes the whole network is healthier than it actually is.

### Well-seeded candidates no longer flood the log

Rejecting a candidate because its seed-scarcity roll failed - the routine, expected outcome for any well-seeded torrent - is no longer logged. At full-catalog scale that was hundreds of `evaluated candidate ... reason="seed-scarcity roll failed"` lines per scan, drowning out the lines that actually matter. Every other outcome (added, swapped, margin failure, RAM cap) still logs.

### `min_seed_margin` defaults to 2

The swap-specific margin (how many fewer seeds a candidate needs before displacing a held torrent) defaults to 2 rather than 3. With selection anchored to the catalog floor, the margin is the guard on top of that global gate, and 2 keeps keep-at willing to replace a held torrent when the gap is real without demanding an increasingly rare margin as catalog health improves.

### One last tracker-announce dispatcher crash fixed

keep-at now builds against `github.com/tweedge/anacrolix-torrent` at tag `v1.61.0-patch2`. A live node hit `122 > 121` (`trackerAnnounceHead.Len()` vs `len(trackerClients)`) during a started announce: the fork's `recover` kept the process alive, but the announce was aborted and the panic recurred. `v1.61.0-patch2` demotes every remaining dispatcher consistency assertion - length-desync checks, infohash-concurrency reconciliation, the per-tracker concurrency limit, and nil-client/URL sanity checks - to warnings with graceful handling, closing out the last known dispatcher crash. See docs/DESIGN.md's "A library bug that took the whole process down" for the full story.

## v0.7.3-beta - stop cleanly, however keep-at was started

This is a beta release for field validation of the shutdown fixes below; the next stable cut will be identical apart from the version tag.

### `keep-at stop` now stops every way keep-at can be running

Previously `keep-at stop` only consulted the PID file, so it reported "daemonctl: not running" for an instance started with `keep-at run` in the foreground, or one running under systemd - both of which don't write a PID file. It now falls back to scanning the process table for such an instance (the same lookup `keep-at status` already used) and signals it, so `keep-at stop` works whether keep-at was daemonized, run in the foreground, or managed by a service.

### `service keep-at stop` no longer hangs mid-scan

The idle path always exited promptly, but if a scan was in flight the scan's drain loop blocked until *every* evaluation worker finished - so a single stuck worker (e.g. a pathological torrent whose metadata parse or tracker scrape never returns) held SIGTERM hostage indefinitely. The drain loop now also aborts on context cancellation, so an interrupted scan shuts down promptly. The systemd unit also gained `TimeoutStopSec=30` as a backstop, so `systemctl stop keep-at` can never block forever even if a future hang bypasses the context handling.

## v0.7.2 - name the torrent that's stalling

The "scrape in progress" progress log now names every candidate currently being evaluated, longest-stalled first, with how long each has been in flight:

```
scrape in progress processed=1500 total=2800 percent=53% elapsed=1h2m eta=54m currently_scraping=["Some Problem Dataset" (12m30s) "Another Dataset" (2m1s)]
```

A pathological torrent - one with a huge piece or file count, or a tracker that never answers - now shows up in the log with its title and how long it's been stuck, instead of the scan just silently slowing down. This is a diagnostic aid while investigating an intermittent high-CPU/high-RAM stall during scrapes; it changes no scan behavior.

This release was previously published as v0.7.2-beta for field validation; this stable v0.7.2 is identical apart from the version tag.

## v0.7.1-beta - fix the crashes and give self-update a beta channel

This is a beta release, published from the same commits the next stable release will come from. It exists so the crash fixes below can be validated in the field before the stable cut.

### Two process-killing bugs fixed (via the anacrolix/torrent fork)

keep-at now builds against `github.com/tweedge/anacrolix-torrent` at tag `v1.61.0-patch1` - upstream v1.61.0 plus two backported crash fixes:

- **Webseed desync panic** (anacrolix/torrent #1036): `updateWebseedRequests` used to panic on a client/torrent request-view desync, from a background timer goroutine nothing can recover from. It's now a warning that skips the update cycle.
- **Tracker-announce dispatcher crash** (`sync: Unlock of unlocked RWMutex`): the announce path released the client lock around the network call, so a panic in that window unwound with the lock released and the caller's deferred `unlock()` hit an already-unlocked mutex - a runtime fatal error, not a recoverable panic. The fork now always re-acquires the lock on exit (including on panic), guards against a missing tracker client, recovers panics on the announce and timer goroutines, and demotes the dispatcher's churn-induced desync assertions to warnings (per the upstream guidance in vintikzzz/torrent 2cc55b5).

The earlier download rate-limiter fix (stale `ReserveN` timestamps letting sustained throughput exceed the configured rate) is carried in the same fork. See docs/DESIGN.md for the full story.

### Interrupted scans don't defer the next one

A scan cut short by ctrl+c or a shutdown signal is no longer persisted as complete. Previously that set `ScanCompletedAt`, so the next start waited out the remainder of the scan interval before scanning again; now an aborted scan stays "in progress" and a restart scans immediately.

### `self-update --beta`

`keep-at self-update` now takes an optional `--beta` flag:

```
keep-at self-update          # stable releases only (default)
keep-at self-update --beta   # allow prerelease/beta builds
```

The default only considers non-prerelease releases, so this beta can't be pulled in accidentally.

## v0.7.0 - make every byte count, and don't hold a slot for a torrent that can never finish

### Post-compression space accounting

keep-at's space accounting now measures **actual on-disk bytes**, not nominal torrent sizes. Pieces are stored gzip-compressed, so a 100 GB torrent that lands on disk as 60 GB consumes only 60 GB of a storage location's limit - the 40 GB of compression gains count toward free space and are used to fit more torrents. Academic Torrents is full of textual and structured data that compresses well, so a location typically fits noticeably more than its nominal-size accounting would suggest.

The same rule runs through every path that fits a torrent:

- **Placement** estimates a candidate's on-disk footprint by applying the location's observed compression ratio (on-disk bytes / nominal bytes across held torrents) to its nominal size.
- **Swaps** measure a displaced torrent's freed space in actual on-disk bytes, so compression gains help in eviction math too.
- **Runtime stats** (`keep-at status`, the periodic summary) report disk used as actual on-disk bytes, so the status line matches what the disk really holds.

Free-space checks are additionally capped by what the device actually reports free (statfs), so filesystem block slack and metadata can never let accounting drift past real capacity.

### New `limit: all` for dedicated drives

A storage location's limit can now be the literal `all` - `limit: all` in a config file, or `--storage-limit all` - which resolves at startup to **97.5% of the device's total formatted capacity**, measured with statfs on the location path. That fraction is deliberately below 100%: filesystems reserve blocks for their own health (ext4 defaults to reserving 5% for root), journals and metadata need room, and block slack on many small piece files costs real space beyond their byte sum.

> **DANGER: dedicated drives only. Never use `all` on an OS drive.** With `limit: all`, keep-at will attempt to fill the device to the resolved fraction - on a system disk that can choke the OS out of space for logs, swap, and the boot process itself. A fixed byte limit is always safer if you're unsure.

### Stalled downloads free themselves

A held torrent that falls to zero seeders and never completes can never finish - with no seeders, nobody can serve its missing pieces - yet it previously held its RAM slot and disk accounting forever, immune to swaps (which only evict *well-seeded* torrents). keep-at now tracks download progress per held torrent (completed pieces, persisted in state) and, after `scan.stall_eviction_timeout` (default **two weeks**, configurable via `--stall-eviction-timeout` or `stall_eviction_timeout`; `0` disables), removes any torrent that has stayed at zero seeders and gained no new pieces.

The stall clock starts at a torrent's first observation and resets whenever it gains a piece, so a torrent gets a full quiet window before any eviction, and slow-but-alive downloads (pieces arriving from leechers' combined data even at zero seeders) are never misclassified as stalled.

## v0.6.1 - see what this host is holding

### New `keep-at hosted-torrents` command

Lists every torrent this host currently holds and seeds, one block per torrent:

```
9111.ru Questions Dataset
  link:        https://academictorrents.com/details/3fa77d9c4028fd6aa8a6dbdad67a218fc1ad7a5d
  status:      seeding
  space:       2.7 GB on disk (torrent is 2.7 GB)
  last scrape: 2 seeders, 0 leechers
```

For each torrent it shows the title, a link to its Academic Torrents page, whether it's currently seeding or still downloading, the actual space it occupies on disk (compared with its full size), and its last-scrape seeder/leecher counts. Like `status` and `network-status`, it reads keep-at's persisted files, so it works whether or not the daemon is running. Status is derived from the storage layout: a torrent is seeding once its final compressed pieces are all on disk with no in-progress pieces left in staging.

### One "evaluated candidate" log line per candidate

A candidate that failed its free-space-fill roll used to fall through to the swap path and log `evaluated candidate` twice - two identical lines for the same torrent with the same seeders. The fill and swap attempts still each run their own selection roll, but the decision is now logged exactly once per candidate, after both attempts have been made.

### Unified `--config` flag across inspection commands

`status`, `network-status`, `stop`, and the new `hosted-torrents` all previously took a `--data-dir` flag in addition to `--config`. They now depend on `--config` alone, resolving the data directory from the config file - and still working with no arguments at all when keep-at is installed as a service.

## v0.6.0 - seed the torrents that need it

keep-at exists to seed *minimally-seeded* torrents - not to put a copy on everything. That's now what the selection logic does, and restarting keep-at no longer re-scrapes the whole catalog needlessly.

### Selection is gated on total seeders, not keep-at peer count

Selection previously gated on how many other keep-at nodes were in a torrent's swarm (`n = aggressiveness ^ keep-at-peers`). With zero keep-at nodes present - the usual case - that roll always succeeded, so keep-at grabbed torrents with a dozen seeders that were already perfectly healthy.

Selection now gates on the torrent's **total seeder count**:

```
n = aggressiveness ^ (seeders - 1)
```

- **1 seeder** is keep-at's primary target and passes with probability 1.
- As seeders grow, the chance shrinks toward zero: 3 seeders -> 0.36, 12 seeders -> 0.36%, 20+ -> effectively never.

A well-seeded torrent is no longer worth a slot, even when disk is free. The keep-at peer count is still gathered, logged per candidate, and reported by `keep-at network-status` - but it no longer drives selection; it's network-status metadata, as the design always intended.

### Restarting no longer triggers an immediate re-scrape

`keep-at run` used to scan immediately on every start. If the last scan completed recently (e.g. you restarted the next morning), keep-at now waits out the remainder of the scan interval instead of re-scraping the whole catalog. It reads the last completion time from `network-stats.json`, so a stale completion, a first run, or a previous process that died mid-scan still scans right away.

## v0.5.4 - portable foreground detection, honest transfer stats, age-gate off switch

### `keep-at status` detects foreground runs on every platform

`keep-at status` previously reported "not running" for an instance started with `keep-at run` in the foreground, because only daemonized `keep-at start` writes a PID file. It now scans the process table and reports `keep-at is running in the foreground (pid N), not as a service` when it finds one using the same data dir.

On macOS and Windows, where the Linux `/proc` process scan isn't available, status falls back to a portable liveness check: the engine writes `runtime-stats.json` into the data dir at startup and every `stats_interval`, so a recently-updated file means keep-at is running there. Those platforms report the foreground instance without a PID.

### Transfer stats now separate useful data from total network traffic

"Downloaded since boot" previously counted every piece chunk received over the wire, including duplicate/wasted chunks from the swarm - which is why a naive reading could show far more "downloaded" than what ended up on disk. keep-at now reports both figures, in the periodic runtime-stats log and in `keep-at status`:

- **useful** - the piece data that actually mattered (bytes sent to peers that requested them; bytes received that keep-at needed),
- **total network** - everything over peer connections since boot, useful payload plus protocol overhead, handshakes, and duplicate/wasted chunks. The gap is the cost of swarming.

Both figures also come with **average rates since boot** (total-network bytes over uptime, in bps/kbps/mbps/gbps), so the log and status show e.g. `uploaded_useful=5.0G uploaded_total=5.2G upload_rate_avg=500 kbps`.

### Age gate can be fully disabled

`moderation_delay: 0` (or `--moderation-delay 0`) now disables the moderation-age gate entirely, including letting through torrents with no posted creation date. Previously a zero `createdAt` was always treated as ineligible even when the delay was off.
