# Configuration reference

Every setting below has both a YAML key (for a config file) and a CLI flag. Flags always take a `--` prefix (e.g. `--port`); YAML keys are lowercase with underscores, nested where noted.

If you're just getting started, you probably only need `--storage-limit` (and optionally `--storage`) - see the README's Quick Start. This page documents everything else for when you want more control.

Precedence, when more than one source could set a value:

1. An explicit CLI flag always wins.
2. Otherwise, a config file (`--config PATH`, or the one `service install` wrote to `/etc/keep-at/config.yaml`) is used if present.
3. Otherwise, keep-at's built-in defaults apply.

## Storage

### `storage.locations` (config file only)

A list of `{path, limit}` pairs. Each `limit` is a fixed integer followed by `M`, `G`, `T`, or `P` (binary units - `1G` is `1024^3` bytes, not `1000^3`). There's no default limit; keep-at always requires at least one explicit location with a positive limit before it will run.

```yaml
storage:
  locations:
    - path: /mnt/disk1/keep-at
      limit: 500G
    - path: /mnt/disk2/keep-at
      limit: 2T
```

keep-at fills multiple locations proportionally to free space, not sequentially, so they fill up roughly evenly over time instead of one disk taking everything until it's full. See [DESIGN.md](DESIGN.md) for the weighting logic.

Multiple locations are a config-file-only feature. The CLI flags below manage exactly one location; combining `--storage`/`--storage-limit` with `--config` is rejected outright (edit the file instead).

### `--storage` (CLI only)

The single storage location to use, when not using a config file. Defaults to an OS-appropriate location:

* Linux: `$XDG_DATA_HOME/keep-at/storage`, or `~/.local/share/keep-at/storage` if `XDG_DATA_HOME` isn't set
* macOS: `~/Library/Application Support/keep-at/storage`
* Windows: `%LOCALAPPDATA%\keep-at\storage`

### `--storage-limit` (CLI only)

How much space `--storage` is allowed to use, e.g. `500G` or `2T`. Required whenever you're not using a config file - keep-at will not guess this.

## Data directory

### `data_dir` / `--data-dir`

Where keep-at keeps its own bookkeeping: persisted state (what it's currently holding), the PID/log files `start`/`stop`/`status` use, cached `.torrent` files and catalog data, and network-status snapshots. This is separate from `storage.locations`, which is only for the torrent data itself.

Defaults to the same OS-appropriate base directory as `--storage` (see above), under `.../keep-at` rather than `.../keep-at/storage`.

## Scanning behavior

### `scan.interval` / `--scan-interval`

*Default: `168h` (one week).* How often keep-at rescans the full Academic Torrents catalog. A scan can take a while on a large catalog - see DESIGN.md - so shortening this a lot mostly just means overlapping or back-to-back scans, not more frequent decisions.

### `scan.rate_limit_per_second` / `--rate-limit`

*Default: `0.5`.* Caps requests specifically to Academic Torrents' own infrastructure: the catalog file, `.torrent` downloads, and their tracker's scrape endpoint. Third-party trackers listed inside a `.torrent` file aren't rate-limited by keep-at, since they aren't Academic Torrents' infrastructure to protect.

### `scan.min_seed_margin` / `--min-seed-margin`

*Default: `3`.* How many fewer seeds a candidate torrent needs, relative to a held torrent (or torrents), before keep-at will displace it to make room. Higher values make keep-at more conservative about swapping; `0` means any strictly-lower seed count qualifies.

### `scan.moderation_delay` / `--moderation-delay`

*Default: `168h` (one week).* Minimum age (from the `.torrent` file's creation date - see DESIGN.md for why not an upload date) before keep-at will consider downloading a torrent. Gives Academic Torrents' moderators time to catch anything that shouldn't be there. Set to `0` to disable the age gate entirely, which also lets torrents with no posted creation date through.

### `aggressiveness` / `--aggressiveness`

*Default: `0.6`.* Must be strictly between 0 and 1. Base of the anti-cascade probability `aggressiveness ^ (other keep-at nodes already in the swarm)` - lower values make keep-at back off faster as more keep-at nodes pile onto the same torrent. See DESIGN.md for the full explanation and the math.

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

keep-at's memory use scales with how many torrents it holds, not with their total size - the underlying BitTorrent library gives every held torrent its own independent pool of peer-connection buffers. So `max_ram` translates into a hard cap on the **number of torrents** keep-at will ever hold at once (logged at startup as `max_torrents`). This is what lets a tiny 1 GB Raspberry Pi on a big disk still seed usefully: it just holds fewer, and (when RAM is the binding constraint rather than disk) larger torrents, getting more bytes seeded per scarce RAM slot. Lowering this also lowers keep-at's connection settings automatically-derived footprint.

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
keep-at run --api-key 'uid=12345;pass=abcdef...' --storage-limit 500G
```

or in a config file:

```yaml
api_key: uid=12345;pass=abcdef...
```

## Network

### `port` / `--port`

*Default: `37550`* (picked randomly during development, checked against common well-known ports). The BitTorrent listen port. If you're running keep-at behind a VPN or router with port forwarding, this is the port to forward - see [VPN.md](VPN.md).

## Throttling

### `upload_rate_limit` / `--upload-rate-limit` and `download_rate_limit` / `--download-rate-limit`

*Default: `0` (unlimited).* Caps how fast keep-at transfers data, in bytes per second, e.g. `50M` = 50 MiB/s. A limit applies **across all torrents at once** - one shared limiter on the torrent client, not a per-torrent budget - so `upload_rate_limit: 10M` means keep-at will never upload faster than 10 MiB/s total. Values use the same size syntax as storage limits (`M`/`G`/`T`/`P`), or a plain byte count; `0` means unlimited. Set both in a config file, or one via flags:

```
keep-at run --upload-rate-limit 50M --download-rate-limit 20M
```

```yaml
upload_rate_limit: 50M
download_rate_limit: 20M
```

## Runtime statistics

### `stats_interval` / `--stats-interval`

*Default: `30m`.* How often keep-at logs a brief summary of what it's doing - torrents held/seeding/downloading, disk utilization, transfer since boot (both useful payload and total network traffic, with average rates), active peers, and uptime - and writes that same summary to disk (in `data_dir/runtime-stats.json`) so `keep-at status` can display it. A summary is always written once at startup; `stats_interval: 0` disables the periodic ones. The log line looks like:

```
runtime stats kind=periodic held=12 seeding=10 downloading=2 disk_used=50.0G disk_limit=100.0G disk_used_pct=50 uploaded_useful=5.0G downloaded_useful=1.0G uploaded_total=5.2G downloaded_total=1.4G upload_rate_avg=500 kbps download_rate_avg=100 kbps peers=24 uptime=2h0m0s
```

and `keep-at status` prints the same picture:

```
keep-at is running (pid 12345)
runtime stats (as of 2026-08-08 20:55:00 UTC, uptime 2h0m0s):
  torrents: 12 held, 10 seeding, 2 downloading
  disk: 50.0 GB used of 100.0 GB configured (50.0%)
  useful upload since boot: 5.0 GB (total network 5.2 GB)
  useful download since boot: 1.0 GB (total network 1.4 GB)
  avg upload since boot: 500 kbps
  avg download since boot: 100 kbps
  active peers: 24
```

**Useful** transfer is the piece data that actually mattered: bytes sent to peers that requested them, and bytes received that keep-at needed. **Total network** is everything that moved over peer connections since boot - useful payload plus protocol overhead, handshakes, and duplicate/wasted chunks received from the swarm. The gap between the two is the cost of swarming, which is why a naive "downloaded" figure can far exceed what actually ended up on disk. The average rates are total-network bytes since boot divided by uptime, in bits per second.

Disk utilization is measured against keep-at's **configured storage limits** (the `storage.locations`/`--storage-limit` totals), not raw filesystem usage - 100% means keep-at has reached the limit it was given. "Since boot" means since this keep-at process started.

## Flags that aren't config fields

A few flags control CLI behavior rather than keep-at's own settings, and don't have a YAML equivalent:

* `--config PATH` - use a config file (see precedence above).
* `--foreground` (`start` only) - run attached instead of daemonizing. Implied automatically inside a container.
* `--user` (`service install` only) - which user the systemd unit runs as (default `root`).
* `--data-dir` (`stop`/`status`/`network-status` only) - override where to look for a running instance's PID/log/state, when you're not using `--config` and haven't installed keep-at as a service.
