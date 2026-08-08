keep-at now bounds its own RAM use and scales how many (and which) torrents it holds to fit the machine it's running on - so a tiny 1 GB Raspberry Pi on a big disk seeds just as sensibly as a server with 32 GB. This release also makes scans dramatically faster and start seeding sooner.

## New: faster scans, seeding starts immediately

The initial full-catalog scrape used to take hours and left configured storage idle the whole time. Three changes fix that:

- **Scrape results are cached across scans.** Each torrent's seeder/leecher counts are remembered on disk (TTL = the scan interval). A weekly re-scan reuses last week's counts instead of re-querying Academic Torrents' tracker for every catalog item, so repeat scans cost almost nothing beyond the torrents keep-at actually chooses to seed.
- **Swarm probing moved to decision time.** The anti-cascade probe (which waits several seconds per candidate to count other keep-at nodes) used to run for every available candidate during evaluation. It now runs only for the candidates keep-at is about to act on, so probe time drops from "every available candidate" to "everything it decides to seed."
- **Incremental acting.** Candidates stream out of evaluation as they complete, and keep-at seeds the highest-priority ones right away - the top of the running ranking fills as soon as it's known, instead of after the whole catalog is walked. It never seeds something that isn't genuinely among the best it could hold (the acted window is capped at the RAM-derived torrent limit), so the "seed the most urgent torrents" guarantee still holds.

Net effect: the most urgent torrents start seeding within minutes of a scan starting, and repeat scans are nearly free.

## New: RAM-aware holding

- Added a `--max-ram` flag (and `max_ram` config key) for the most memory keep-at will plan around. It defaults to **80% of system RAM**, and an explicit value is never allowed to exceed that hard cap (keep-at refuses to start rather than overcommit the host).
- RAM scales with the **number** of torrents keep-at holds, not their total size: the underlying BitTorrent library gives every held torrent its own independent pool of peer-connection buffers. So `max_ram` translates into a hard cap on the torrent count, logged at startup as `max_torrents`. A free-space fill that would exceed the cap is skipped; only swaps (net-zero or negative count) proceed.
- When RAM is the binding constraint rather than disk, the decision function flips to **prefer larger torrents**: more bytes get seeded per scarce RAM slot, and eviction sheds the smallest held torrents first so big ones survive. This is exactly the "prioritize larger torrents to fill space when RAM runs out before disk" behavior - a 1 GB Pi on a 20 TB disk holds fewer but bigger torrents and stays effective.

## Changed: lower default connection footprint

keep-at is a seed-first, always-on daemon that assumes many keep-at nodes share the Academic Torrents seeding load, so no single node needs to be a speed monster. The default per-torrent peer fanout and buffer sizes are now much smaller (12 established connections per torrent instead of 50, 256 KiB per-connection buffer instead of 1 MiB). This cuts the dominant RAM cost by roughly 4x and is what makes the RAM budget meaningful on small boxes.

## Planning notes

- The per-torrent footprint estimate (`EstablishedConnsPerTorrent × MaxAllocPeerRequestDataPerConn + a fixed base`) is a deliberate conservative upper bound used only for planning the torrent-count cap, not a precise live allocation accounting.
- `max_ram` is honored from both `--max-ram` and a config file's `max_ram`; the 80%-of-system hard cap applies to both.

Real full-catalog testing surfaced crashes and scaling problems that a small hand-picked test never hit. This release fixes all of them and adds visibility into how long a scrape is actually taking.

## Fixes

- **Crash fix**: pinned `github.com/anacrolix/torrent` to v1.60.0. v1.61.0's rewritten tracker announce dispatcher has a locking bug that surfaces as an unrecoverable Go runtime fatal error ("sync: Unlock of unlocked RWMutex"), triggered from more than one code path under the torrent churn a real scan produces.
- Swarm probing now runs on its own disposable, DHT-disabled torrent client instead of adding and dropping torrents on the main one - that add/drop churn was an independent trigger for bugs in the same family, regardless of library version. DHT stays enabled on the main client used for real downloads, since disabling it globally measurably hurt download connectivity.
- Candidates are now evaluated concurrently (16 at a time). Swarm probing alone can take several seconds per candidate; run sequentially across a catalog with thousands of them, a scan would take hours longer than it needs to.
- The torrent client's automatic tracker re-announcing - separate from keep-at's own rate-limited scrapes - was enough on its own to get keep-at rate-limited (HTTP 429) by Academic Torrents in real testing. It now shares the same rate limit.
- Replaced the panicking `TorrentSpecFromMetaInfo` with the non-panicking `TorrentSpecFromMetaInfoErr` throughout: a decade-plus of inconsistent uploads means a malformed `.torrent` file is an expected case, not an exceptional one.
- Added a recover()-based safety net around per-candidate work, as defense in depth against the (much larger) class of ordinary panics that inconsistent metadata can trigger.
- Quieted an overwhelming amount of expected log noise (dead third-party trackers, malformed webseed URLs) from the underlying torrent library, without touching keep-at's own logging.

Verified with a real, unbounded run against the entire live Academic Torrents catalog (2,850 items) capped at 10GB in /tmp: stable for 20+ minutes and 200+ candidates processed with zero crashes, versus crashing within about a minute before this release.

## New: scrape progress logging

A full-catalog scrape can run for a long time, and previously the log went quiet for the whole thing. Three log lines now mark the phase:

- `"starting scrape"` once, up front, stating explicitly that it can take a while and that keep-at won't add, swap, or remove anything it holds until the scrape finishes.
- `"scrape in progress"` every 2 minutes while it runs, with percent complete and a human-readable ETA (e.g. `2h15m`, `5m30s`).
- `"scrape complete, updating what keep-at holds"` once it finishes, right before keep-at acts on the results.

See docs/DESIGN.md for the full writeup of all of the above, under "Running against the real catalog" and "Reporting progress during a long scrape."
