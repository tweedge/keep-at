# Configuration reference

Every setting below has both a YAML key (for a config file) and a CLI flag. Flags always take a `--` prefix (e.g. `--port`); YAML keys are lowercase with underscores, nested where noted.

If you're just getting started, you probably only need `--storage-location` and `--storage-limit` - see the README's Quick Start. This page documents everything else for when you want more control.

Precedence, when more than one source could set a value:

1. An explicit CLI flag always wins.
2. Otherwise, a config file (`--config PATH`, or the one `service install` wrote to `/etc/keep-at/config.yaml`) is used if present.
3. Otherwise, keep-at's built-in defaults apply.

## Storage

### `storage` (config file)

A list of `{path, limit}` pairs. Each `limit` is either a byte size (`500G`, `2T` - binary units, `1G` is `1024^3` bytes), or the literal `max` (`all` still works as a deprecated alias) - see below. There's no default limit; keep-at always requires at least one explicit location with a positive limit before it will run.

```yaml
storage:
- path: /mnt/disk1/keep-at
  limit: 500G
- path: /mnt/disk2/keep-at
  limit: 2T
```

Limits are enforced against nominal torrent sizes plus a small per-torrent buffer (for the cached `.torrent` file and state overhead): a 100 GB torrent consumes ~100 GB of the limit regardless of how compressible its bytes are. Limits under 100M are rejected outright (certainly a units mistake - did you mean gigabytes?); limits under 1G warn but proceed. Free-space checks are also capped by what the device actually reports free, so filesystem block slack and metadata can't push real usage past capacity, and keep-at never exceeds a location's limit.

`limit: max` (or `--storage-limit max`) resolves at startup to 97.5% of the storage device's total formatted capacity, measured with statfs on the location path - the device is dedicated to keep-at and the last 2.5% (plus whatever the filesystem reserves) is left for the journal, metadata, and the OS's emergency operations. **Only use `max` on a dedicated data drive.** On an OS drive, keep-at will attempt to fill the device to that fraction and can choke the OS out of room for logs, swap, and the boot process. A fixed byte limit is the safe choice whenever the drive isn't exclusively keep-at's.

keep-at fills multiple locations proportionally to free space, not sequentially, so they fill up roughly evenly over time instead of one disk taking everything until it's full. See [DESIGN.md](DESIGN.md) for the weighting logic.

Multiple locations work from flags too: repeat `--storage-location PATH --storage-limit SIZE` pairs for as many drives as you have, and keep-at fills them proportionally to free space. Combining storage flags with `--config` is rejected outright (edit the file instead).

### `--storage-location` / `--storage-limit` (CLI only)

Repeatable pairs configuring any number of storage locations without a config file. The single-location `--storage` shorthand pairs with one `--storage-limit` the same way.

If no storage flag is passed, keep-at uses `~/.local/share/keep-at/storage` (`/var/lib/keep-at/storage` when `HOME` is unset), but still requires an explicit `--storage-limit` - it will not guess how much space to take.

## Data directory

### `data_dir` / `--data-dir`

Where keep-at keeps its own bookkeeping: persisted state (what it's currently holding), the PID/log files `start`/`stop`/`status` use, cached `.torrent` files and catalog data, network-status snapshots, and the live query socket (`keep-at.sock`, created while the daemon runs). This is separate from `storage`, which is only for the torrent data itself.

Defaults to `~/.local/share/keep-at` (`/var/lib/keep-at` when `HOME` is unset).

Read-only operations are readable by every local user: `status` and `hosted-torrents` query the running daemon over the query socket when it's up (falling back to the persisted files when it isn't), and the log file works for any user against a daemon owned by anyone (including a root-run service), because snapshots, caches, state files, the PID file, the socket, the log, and the config file itself are written world-readable (0o644 files / 0o666 socket, umask-independent) and the daemon repairs directory traversal (0o755 masked in) on startup. Writes stay owner-only — a non-owner second `run` against the same data dir fails on permissions, as it should. The one exception is the API key: it lives in `<data_dir>/api_key`, owner-only (0o600), since it is passkey-equivalent. Older installs (pre-0.8.11) kept the whole config at 0o600 for the same reason; the daemon migrates that shape on its first start after upgrading (key extracted to the file, config opened up), or `service install` re-run does it immediately.

## Scanning behavior

### `scan.interval` / `--scan-interval`

*Default: `168h` (one week).* How often keep-at rescans the full Academic Torrents catalog. A scan can take a while on a large catalog - see DESIGN.md - so shortening this a lot mostly just means overlapping or back-to-back scans, not more frequent decisions.

### `scan.rate_limit_per_second` / `--rate-limit`

*Default: `0.5`.* Caps requests specifically to Academic Torrents' own infrastructure: the catalog file, `.torrent` downloads, and their tracker's scrape endpoint. Third-party trackers listed inside a `.torrent` file aren't rate-limited by keep-at, since they aren't Academic Torrents' infrastructure to protect.

### `scan.min_seed_margin` / `--min-seed-margin`

*Default: `2`.* How many fewer seeds a candidate torrent needs, relative to each held torrent it would displace (or the least-seeded of a displaced set), before keep-at will displace it to make room. Higher values make keep-at more conservative about swapping; `0` means any strictly-lower seed count qualifies. This is the swap-specific guard on top of the global seed-scarcity gate - see DESIGN.md's "Seeding minimally-seeded torrents" section for how the two interact.

### `scan.moderation_delay` / `--moderation-delay`

*Default: `168h` (one week).* Minimum age (from the `.torrent` file's creation date - see DESIGN.md for why not an upload date) before keep-at will consider downloading a torrent. Gives Academic Torrents' moderators time to catch anything that shouldn't be there. Set to `0` to disable the age gate entirely, which also lets torrents with no posted creation date through.

### `scan.stall_eviction_timeout` / `--stall-eviction-timeout`

*Default: `336h` (two weeks).* How long a held torrent can sit with **zero seeders and no download progress** before keep-at removes it to free the slot and disk for a torrent that can actually complete. Every scan refreshes how many pieces a held torrent has stored; a torrent that gains no new pieces for this entire timeout while having zero seeders can never finish (no one can serve its missing pieces), so it's evicted. The clock starts at a torrent's first observation and resets whenever it gains a piece, so slow-but-alive downloads are never evicted. Set to `0` to disable stalled-torrent eviction entirely.

### `aggressiveness` / `--aggressiveness`

*Default: `0.6`.* Must be strictly between 0 and 1. Base of the seed-scarcity probability `aggressiveness ^ max(0, seeders - x)` - keep-at's purpose is to seed minimally-seeded torrents, so this is the chance it proceeds with a candidate given how many seeders it already has relative to the catalog's p10 seeder floor x (measured by the last completed scan; effectively 1 before any scan has run). A torrent at or below the floor passes with probability 1; a torrent with many seeds above it is effectively never selected. Lower values make keep-at even more selective. See DESIGN.md for the full explanation and the math.

## Filtering

### `keyword_blocklist` / `--keyword-blocklist`

*Default: none.* Case-insensitive substring match against a torrent's title and description (the only text fields Academic Torrents' bulk catalog file provides). Anything matching is skipped before any network calls are made for it. As a YAML list:

```yaml
keyword_blocklist:
  - confidential
  - draft
```

From the CLI, pass a comma-separated list: `--keyword-blocklist confidential,draft`.

### `preserve_deleted_torrents` / `--preserve-deleted-torrents`

*Default: `false`.* If Academic Torrents removes a torrent keep-at is seeding, keep-at removes its local copy too by default, on the theory that a takedown probably happened for a reason. Set this to `true` to keep seeding removed torrents anyway.

## Memory

### `max_ram` / `--max-ram`

*Default: 80% of system RAM.* The most memory keep-at will plan its holding around. keep-at runs unattended and is meant to share a host (a Pi's OS, the arr stack, other services), so by default it spends up to 80% of the machine's physical RAM and figures out the rest itself. You can set a smaller explicit cap (e.g. `--max-ram 1G`), but never a larger one: keep-at refuses to use more than 80% of system RAM, and asks beyond that are rejected at startup.

keep-at's memory use scales with how many torrents it holds and how many *pieces* they have - the underlying BitTorrent library (rqbit) keeps per-torrent bookkeeping of ~256 KiB plus ~64 B per piece, plus per-live-peer buffers sized by the session's peer limit - not with their total byte size. So `max_ram` translates into a cap on the **number of torrents** keep-at will ever hold at once (logged at startup as `max_torrents`, priced at ~1k pieces each). This is what lets a tiny 1 GB Raspberry Pi on a big disk still seed usefully: it just holds fewer, and (when RAM is the binding constraint rather than disk) larger torrents, getting more bytes seeded per scarce RAM slot. Two mechanisms make the RAM/storage ratio work:

- **RAM-scaled peer limit** (logged at startup as `peer-limit`): 20 peers/torrent on comfortable hosts, stepping down to 12 / 8 / 4 as the budget shrinks. Peer buffers are the dominant per-torrent term, so a 512 MiB node holds ~600 torrents instead of ~100 - at the cost of slower per-torrent swarms, the right trade for a seeder whose torrents are mostly complete.
- **Bytes-per-RAM ranking and swap pricing**: when RAM-bound, candidates rank by bytes seeded per RAM byte (a 2 GiB 100-piece torrent outranks a 2 GiB 100k-piece one), free-space fills check remaining RAM headroom before adding, and swaps only displace a set whose combined footprint covers the candidate's. A many-piece giant can never push RSS past the budget, even with terabytes of free disk.

## Academic Torrents account attribution

### `api_key` / `--api-key`

*Default: unset (anonymous).* This setting is **completely optional** - if you don't set it, keep-at seeds anonymously and behaves exactly the same in every way. The only thing an API key changes is attribution. Set it to your Academic Torrents API key, from https://academictorrents.com/my.php (it looks like `uid=12345;pass=abcdef...`), and keep-at attributes the torrents it seeds to your account, so the details page of each torrent shows your name and image as one of the users currently hosting the data - that's the "Hosted by" box described in AT's [mirroring docs](https://academictorrents.com/docs/mirroring.html).

At startup, keep-at resolves the key through AT's own `userannounce` endpoint (the same mechanism AT's smartnode tooling uses) into the per-user announce URL carrying your account's passkey, then uses that URL for every announce to AT's trackers. Third-party trackers listed in `.torrent` files are never touched.

Security notes:

- The key is **only ever sent to the two Academic Torrents tracker hosts** (`academictorrents.com` and `ipv6.academictorrents.com`, https only). Any other tracker - or an `http://` or lookalike `*.academictorrents.com` host - never receives it.
- The key and the resolved passkey URL are **never logged, never written to cached `.torrent` files or state, and never surfaced anywhere** except the announce to AT's tracker.
- If the key is invalid or the endpoint is unreachable, keep-at logs a warning (with the secret portion redacted) and keeps running unattributed - it never crashes or refuses to start.

Setting it:

```
keep-at run --api-key 'uid=12345;pass=abcdef...' --storage-location ~/.local/share/keep-at/storage --storage-limit 500G
```

The key is stored in `<data_dir>/api_key` (owner-only, `0600`) — not in the config file, which stays world-readable so `status`/`hosted-torrents` work for any user. Setting `--api-key` on any start (or `service install`) writes the file; removing it (or emptying the file) reverts to anonymous seeding. A legacy `api_key:` field in a config file still parses for compatibility, but the key file wins when both exist, and the daemon migrates the inline key out on first start.

## Network

### `port` / `--port`

*Default: `37550`* (picked randomly during development, checked against common well-known ports). The BitTorrent listen port. If you're running keep-at behind a VPN or router with port forwarding, this is the port to forward - see [VPN.md](VPN.md).

## Throttling

### `upload_rate_limit` / `--upload-rate-limit` and `download_rate_limit` / `--download-rate-limit`

*Default: `0` (unlimited).* Caps how fast keep-at transfers data, in bytes per second, e.g. `50M` = 50 MiB/s. A limit applies **across all torrents at once** - one shared limiter on the torrent client, not a per-torrent budget - so `upload_rate_limit: 10M` means keep-at will never upload faster than 10 MiB/s total. Values use the same size syntax as storage limits (`M`/`G`/`T`/`P`), or a plain byte count; `0` means unlimited. Absurdly small caps are rejected (<100K, certainly a units mistake - did you mean megabytes?); caps under 1M warn but proceed. Set both in a config file, or one via flags:

```
keep-at run --upload-rate-limit 50M --download-rate-limit 20M
```

```yaml
upload_rate_limit: 50M
download_rate_limit: 20M
```

## Runtime statistics

### `stats_interval` / `--stats-interval`

*Default: `30m`.* How often keep-at logs a brief summary of what it's doing - torrents held/seeding/downloading, disk utilization, transfer since boot (both useful payload and total network traffic, with average rates), active peers, process RSS, and uptime - and refreshes the persisted snapshot in `data_dir/runtime-stats.json` (the offline fallback `status` reads when no daemon is running). A summary is always written once at startup and once after every scan; `stats_interval: 0` disables the periodic ones. When the daemon is running, `keep-at status` reads live numbers straight from it over the query socket instead (instantaneous, never stale). The log line looks like:

```
runtime stats (kind=periodic held=12 seeding=10 downloading=2 disk=50.0 GiB/100.0 GiB up=5.0 GiB down=1.0 GiB peers=24 rss=300.0 MiB uptime=7200s)
```

and `keep-at status` prints the same picture (with a `live` marker when the numbers come straight from the running daemon, or the snapshot timestamp when read from disk while no daemon is running):

```
keep-at is running (pid 12345)
runtime stats (2026-08-08 20:55:00 UTC uptime, uptime 2h0m0s):
  torrents: 12 held, 10 seeding, 2 downloading
  disk: 50.0 GiB used of 100.0 GiB configured (50.0%)
  useful upload since boot: 5.0 GiB (total network 5.2 GiB)
  useful download since boot: 1.0 GiB (total network 1.4 GiB)
  avg upload since boot: 500.0 Kbit/s
  avg download since boot: 100.0 Kbit/s
  active peers: 24
  memory: 300.0 MiB RSS
```

**Useful** transfer is the piece data that actually mattered: bytes sent to peers that requested them, and bytes received that keep-at needed. **Total network** is everything that moved over peer connections since boot - useful payload plus protocol overhead, handshakes, and duplicate/wasted chunks received from the swarm. The gap between the two is the cost of swarming, which is why a naive "downloaded" figure can far exceed what actually ended up on disk. The average rates are total-network bytes since boot divided by uptime, in bits per second.

Disk utilization is measured against keep-at's **configured storage limits** (the `storage`/`--storage-limit` totals), not raw filesystem usage - 100% means keep-at has reached the limit it was given. Disk *used* is actual on-disk bytes under each location. Torrents are stored as plain sparse files, so reported usage tracks nominal sizes closely (unallocated sparse regions cost nothing). "Since boot" means since this keep-at process started.

## Flags that aren't config fields

A few flags control CLI behavior rather than keep-at's own settings, and don't have a YAML equivalent:

* `--config PATH` - use a config file (see precedence above).
* `--data-dir PATH` - override the data directory for this invocation (on `run`/`start`/`stop`/`status`/`hosted-torrents`; wins over the config file).
* `--foreground` (`start` only) - run attached instead of daemonizing. Implied automatically inside a container.
* `--user` (`service install` only) - which user the systemd unit runs as (default `root`).
* `--probe-timeout` (`network-status` only) - how long to wait per torrent for peers while probing its swarm (default `10s`).

## Logging

### `log_file` / `--log-file`

*Default: unset (log to stdout).* Write logs to a file instead of standard output. `start` sets this automatically to `<data_dir>/keep-at.log` when you don't pass one (a detached daemon's stdio is discarded, so without a log file its output would go nowhere); `run` in a terminal leaves it on stdout. The systemd unit captures stdout via the journal instead, so it needs no log file. The log file is created world-readable (like the snapshots below) so any local user can tail it.

### `debug` / `--debug`

*Default: `false`.* Verbose diagnostics: debug-level logging (overridable per-process with `RUST_LOG`, e.g. `RUST_LOG=keep_at::engine=debug`). There is no debug-artifact directory - diagnostics are the log lines themselves plus the persisted files (`network-stats.json`, `runtime-stats.json` offline fallback, `scrape-cache.json`, `state.json`) under the data dir.
