# keep-at release notes

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
