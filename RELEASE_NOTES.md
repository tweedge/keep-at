Real full-catalog testing surfaced crashes and scaling problems that a
small hand-picked test never hit. This release fixes all of them and
adds visibility into how long a scrape is actually taking.

## Fixes

- **Crash fix**: pinned `github.com/anacrolix/torrent` to v1.60.0.
  v1.61.0's rewritten tracker announce dispatcher has a locking bug
  that surfaces as an unrecoverable Go runtime fatal error ("sync:
  Unlock of unlocked RWMutex"), triggered from more than one code path
  under the torrent churn a real scan produces.
- Swarm probing now runs on its own disposable, DHT-disabled torrent
  client instead of adding and dropping torrents on the main one -
  that add/drop churn was an independent trigger for bugs in the same
  family, regardless of library version. DHT stays enabled on the
  main client used for real downloads, since disabling it globally
  measurably hurt download connectivity.
- Candidates are now evaluated concurrently (16 at a time). Swarm
  probing alone can take several seconds per candidate; run
  sequentially across a catalog with thousands of them, a scan would
  take hours longer than it needs to.
- The torrent client's automatic tracker re-announcing - separate
  from keep-at's own rate-limited scrapes - was enough on its own to
  get keep-at rate-limited (HTTP 429) by Academic Torrents in real
  testing. It now shares the same rate limit.
- Replaced the panicking `TorrentSpecFromMetaInfo` with the
  non-panicking `TorrentSpecFromMetaInfoErr` throughout: a
  decade-plus of inconsistent uploads means a malformed `.torrent`
  file is an expected case, not an exceptional one.
- Added a recover()-based safety net around per-candidate work, as
  defense in depth against the (much larger) class of ordinary panics
  that inconsistent metadata can trigger.
- Quieted an overwhelming amount of expected log noise (dead
  third-party trackers, malformed webseed URLs) from the underlying
  torrent library, without touching keep-at's own logging.

Verified with a real, unbounded run against the entire live Academic
Torrents catalog (2,850 items) capped at 10GB in /tmp: stable for
20+ minutes and 200+ candidates processed with zero crashes, versus
crashing within about a minute before this release.

## New: scrape progress logging

A full-catalog scrape can run for a long time, and previously the log
went quiet for the whole thing. Three log lines now mark the phase:

- `"starting scrape"` once, up front, stating explicitly that it can
  take a while and that keep-at won't add, swap, or remove anything
  it holds until the scrape finishes.
- `"scrape in progress"` every 2 minutes while it runs, with percent
  complete and a human-readable ETA (e.g. `2h15m`, `5m30s`).
- `"scrape complete, updating what keep-at holds"` once it finishes,
  right before keep-at acts on the results.

See docs/DESIGN.md for the full writeup of all of the above, under
"Running against the real catalog" and "Reporting progress during a
long scrape."
