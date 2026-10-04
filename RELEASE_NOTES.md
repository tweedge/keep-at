# keep-at release notes

## v0.8.30-beta - broken-piece quarantine: stop silently-broken swarms burning bandwidth forever

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### Broken-piece quarantine (the NotaBug incident shape)

Some swarms are quietly poisoned: the seeders hold data that contradicts the torrent's registered piece hashes (an uploader re-generated a README after creating the torrent — the actual Oct 2026 incident), so every client downloads a piece, fails its hash check, and retries from another peer forever: megabytes per second of download with zero progress. A real swarm ran like that for a week at ~2.3 GiB/h (~55 GiB/day) before anyone noticed.

keep-at now watches the gap between received wire bytes and hash-validated bytes on every live torrent (rqbit's `fetched_bytes` vs `downloaded_and_checked_bytes`, counted since process start). A torrent that burns `scan.broken_piece_discard_bytes` (default `256M`) of received-but-never-validated bytes across `scan.broken_piece_min_windows` consecutive zero-progress watchdog passes (default 2, one pass per `scan.quarantine_check_interval`, default 30m) is a broken swarm. Any verified byte resets the counters, counter regressions (restarts) re-baseline, and in-flight bytes are credited when their piece completes — healthy-but-slow torrents never trip.

A trip removes the torrent (data included) and records a cooldown entry in `<data_dir>/state.json` under `scan.quarantine_cooldown` (default 3d). The selection gate refuses the hash while the cooldown is active — even though the catalog keeps listing it. When the cooldown lapses, the next scan re-adds the torrent as a probe (bypassing the seed-scarcity roll, which would otherwise never re-admit a well-seeded broken swarm) into free space only: a speculative probe never displaces and deletes a healthy held torrent just to test upstream. If upstream fixed their seeders' data, the re-probe completes against the registered piece hashes and the quarantine lifts automatically; if the swarm is still broken it re-quarantines within about an hour and its attempts counter climbs toward `scan.quarantine_max_retries` (default 0 = unlimited; past the limit the cooldown becomes indefinite until you delete the entry from the `quarantined` map in state.json). `keep-at status` shows a `quarantined:` line while any hash is under cooldown; drops are recorded in history with the `quarantined` cause; entries whose hash is delisted, lapsed, and in neither the held set nor the session are garbage-collected automatically. See docs/CONFIG.md ("Broken-piece quarantine") for every knob.

### Scan loop: multi-day waits are no longer cut short by periodic ticks

The post-boot scan wait (up to `scan.interval` after a recently completed scan) used a bare `select!` against the stats ticker, so the first periodic tick fired an early scan ~30 minutes into a multi-day wait — observed on the Sep 21 and Sep 26 boots. The run loop is now a single deadline-driven loop anchored to actual scan completions: ticks no longer end the wait, failed scans retry at the interval floor (never hot-loop), and periodic stats/watchdog passes run during the wait as well as between scans.

### Stall eviction: broken loops can no longer hide behind their seeders

The old rule shielded any incomplete torrent with live seeders from stall eviction forever — exactly how the broken-piece loop sat hidden for weeks. Incomplete torrents with seeders are now evictable after `scan.stall_eviction_timeout` (default 90d) without verified-byte progress, with two protections intact: a fully-present torrent is never evicted regardless of how stale its progress clock looks (rqbit's finished flag covers padding-file torrents, where progress tops out below the registered size), and torrents running an integrity check are exempt (post-crash boots run hundreds of these — without the exemption a boot would evict healthy data en masse). A held torrent the session lost (no cached .torrent) whose data is missing now evicts via a dir-size fallback instead of sitting in the library forever.

### Persistence, validation, and upgrade hardening

- `completed_pieces` in state.json is now a full u64. The u32 cap saturated past 4 GiB and froze the stall clock at every scan for big torrents (every refresh looked like progress), so nothing large was ever stall-evicted. The one-way door bites only once a value above 4294967295 lands: `keep-at self-update` snapshots `state.json.pre-<version>` before replacing the binary, and old binaries refuse to load newer state — see docs/RECOVERY.md before downgrading.
- state.json writes fsync the temp file and parent directory before the rename, so power loss can no longer leave a truncated state file that boot-loops the daemon under the watchdog.
- Duration knobs validate to 0..=366 days and rate limits to >= 1e-6/s at startup — a bad config now fails loudly at boot instead of misbehaving. A YAML `null` on a duration knob no longer silently falls back to `scan.interval`'s default (this used to make `stall_eviction_timeout: null` mean 14 days instead of 90), and configs without a `scan:` section get real defaults instead of a zero scan interval and zero seed margin.
- The quarantine watchdog's missed passes reschedule instead of bursting, so its zero-progress windows always count real intervals.
- `keep-at history` and the history views see the rotated `.1` generation again (trip storms used to rotate events out of every view).

### Upgrade notes

Safe to roll over the running daemon the usual way (watchdog promote, or `keep-at self-update --beta`). No config changes required; the quarantine registry starts empty and is a strict no-op until the first trip. Operators who pinned duration knobs above 366 days (or rate limits below 1e-6/s) must adjust those first — validation now rejects them at boot.
