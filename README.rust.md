# keep-at (Rust/rqbit migration)

A smart node that seeds Academic Torrents. This is the Rust port of keep-at
(Go + anacrolix/torrent), rebuilt on [rqbit](https://github.com/ikatson/rqbit)
(`librqbit` 9.x) for lower memory/CPU use and crash resistance. Linux-only.

## Quick start

```sh
# One-line install (Linux x86_64/aarch64):
curl -fsSL https://raw.githubusercontent.com/tweedge/keep-at/rust-migration/scripts/install.sh | sh

# Or with Docker:
docker run -v ./data:/data -v ./storage:/storage ghcr.io/tweedge/keep-at \
  --storage-limit 500G

# Or from source (needs a Rust toolchain, no OpenSSL dev headers required -
# TLS + hashing use rustls/ring):
cargo build --release
./target/release/keep-at run --storage-limit 500G
```

At minimum: `keep-at run --storage-limit 500G`. A config file is optional;
every setting has a `--flag` (`keep-at run --help`). Repeatable
`--storage-location PATH --storage-limit SIZE` pairs mean multiple drives
never need a config file either.

## Commands

- `keep-at run` - run in the foreground
- `keep-at start` - start as a background process (`--foreground` to not daemonize)
- `keep-at stop` - stop the background process
- `keep-at status` - report whether keep-at is running + runtime summary
- `keep-at service install|uninstall` - systemd service (Linux, root)
- `keep-at network-status` - census the keep-at network (RAM/time-heavy, on demand)
- `keep-at hosted-torrents` - list torrents this host holds and seeds
- `keep-at self-update [--beta]` - update to the latest release
- `keep-at version` - print the version

## How it picks torrents

Every scan, keep-at pulls Academic Torrents' `database.xml`, skips held /
blocked / oversized / too-young torrents, scrapes seeder counts, and ranks
by fewest seeders first. The seed-scarcity gate (`n = aggressiveness ^
max(0, seeders - floor)`, floor = p10 seeder count from the last completed
scan) keeps every node from piling onto the same torrent. Full space fills
with the best candidates; a full disk swaps held torrents for better ones
only when the candidate beats them by `--min-seed-margin` (default 2)
seeds. Zero-seeder torrents with no progress past `--stall-eviction-timeout`
(default 14 days) are freed.

## Storage

Plain on-disk layout: `<location>/<infohash-hex>/` holds the torrent's real
files (sparse), so locations never overlap and data is directly usable.
Accounting uses nominal size + a small per-torrent buffer and never exceeds
a location's limit. `limit: all` (or `--storage-limit all`) resolves to
97.5% of the device's formatted capacity - dedicated data drives only.

## Napkin math: how much RAM per TB of storage

Rule of thumb: **~1 GiB of physical RAM per 1 TB of storage fills the disk;
~60 GiB of physical RAM per 1 TB fills it with 16 MB-average torrents.**
The two numbers differ because RAM cost tracks torrent *count* and piece
count, while disk tracks bytes. Derivation (measured on rqbit 9.0.1,
release profile):

- Per-torrent RAM ≈ 256 KiB base + 64 B/piece + 48 KiB × peer-limit.
  A typical catalog torrent (~1k pieces) costs ~0.7 MiB at peer-limit 8.
- Budget is 80% of physical RAM, so a 1 GiB box plans around ~820 MiB →
  ~1,190 slots.
- What those slots total in bytes depends on *which* torrents urgency
  ranking selects, and that is the whole ballgame:
  - Live 327-torrent node: 16 MB average → 1,190 slots ≈ 20 GB per TB
    of attached disk (~2% utilization of a 1 TB drive).
  - Same 1,190 slots at the 1.72 GB average of the catalog's 600 largest
    entries → ~2 TB. The 1 GiB box fills a 1 TB drive twice over.

So RAM per TB is not a property of the disk - it is a property of the
average torrent size the network needs seeded when your node scans:

| torus profile | slots/TB | RAM per TB of *filled* disk |
|---|---|---|
| 16 MB average (small-torrent mix) | ~67,000 | ~60 GiB physical |
| 1.7 GB average (600 largest catalog entries) | ~600 | ~1 GiB physical |

On a RAM-short box (512 MiB + 1 TB) the disk will sit mostly empty by
design: ~600 slots fill with the largest, fewest-piece torrents urgency
ranking surfaces, and free-space fill refuses anything whose RAM price
exceeds remaining headroom. That is the correct behavior - the alternative
is exceeding the RAM budget and OOMing the host. If the disk must be full,
add RAM, not flags: no selection parameter can hold more torrents than the
budget prices.

`--max-ram` caps the budget below the 80% default (never above). The
startup log prints the resolved `budget`, `peer-limit`, and `max-torrents`
so the arithmetic above is checkable per host.

## Design notes

See `docs/DESIGN.md` for the selection math and operational lessons (much of
it carries over; the torrent library changed from anacrolix/torrent to
rqbit). Key Rust-port differences:

- Scrapes are HTTPS-only (no UDP/BEP-15 fallback); the AT tracker answers
  for AT content.
- No webseeds: rqbit has no webseed support, and keep-at seeds from real peers.
- No compression backend: plain files, nominal-size accounting.
- Linux only: no macOS/Windows builds, no launchd/SCM service support.
- State files are new (clean break, no Go state import): `state.json`,
  `network-stats.json`, `runtime-stats.json`, `scrape-cache.json`,
  `torrent-cache/` under the data dir.

## Development

```sh
cargo test            # unit tests (offline)
KEEPAT_SMOKE_TEST=1 cargo test --test smoke   # fast live pipeline test
KEEPAT_SMOKE_SUBSET=1 cargo test --test smoke # real-catalog subset scan
```
