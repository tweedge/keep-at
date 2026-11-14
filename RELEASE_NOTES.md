# keep-at release notes

## v1.0.0 - the Rust build is the stable line

keep-at 1.0.0 is the first stable release since v0.7.2, and it promotes the Rust-on-[rqbit](https://github.com/ikatson/rqbit) build - developed and field-validated across the v0.8.x beta series - to the supported stable line. Every v0.8.x beta is covered by this cut.

### The core of it

- **Rewritten in Rust on rqbit.** One binary for run/start/stop/status, self-update, systemd `service install`, multiple storage locations, Academic Torrents API-key attribution, and the seed-scarcity selection gate. Every user-facing behavior from the Go build carries over; state files are new (a clean break - the Go build's v0.7.2 state is not imported).
- **Vendored rqbit instead of crates.io.** `librqbit`, `librqbit-bencode` and `librqbit-core` build from a vendored branch of our rqbit fork (upstream mainline v9.0.1 + 25 fixes, plus our three open upstream PRs): the peers map is now bounded (#667), deferred integrity checks and an on-demand recheck API exist (#670), and BEP-52 hybrid `.pad/<length>` filenames are accepted (#701). See `docs/VENDORING.md`.
- **Memory is bounded by construction.** A measured per-torrent RAM model drives selection and peer limits; jemalloc returns freed pages to the OS continuously; DHT is off (heap profiling traced the steady-state leak to `request_peers_forever`); and the recommendation dropped to 512 MiB base + ~128 MiB per TB of storage after measured DHT-off nodes sat well inside the old line.
- **Library safety.** A catalog-collapse guard refuses a removal pass on a suspiciously shrunken catalog, vanished-from-catalog torrents get a 90-day grace period before eviction, broken-piece quarantine watches discarded bytes and re-probes recovered swarms on the watchdog cadence, and the seed-scarcity gate no longer admits swaps it cannot fill.
- **Operations.** Live `status` over a query socket (with persisted-file fallback), `hosted-torrents`, `logs` (follow or replay), and `history` (additions/swaps/drops); world-readable snapshots with the API key split out owner-only; fd-limit handling that avoids EMFILE wedges; SIGPIPE and daemon-death forensics fixes; and native elevation for `service install` / `self-update`.

### Versioning and self-update: stable tags are now `x.y.z`

Stable releases are now three components (`v1.0.0`) and development builds are the same plus `-beta` (`v1.0.1-beta`); legacy two-component tags still classify (`v0.7.2` stable, `v0.8.8-beta` beta). This is a change from the previous "two components stable" rule, which would have classified a bare `v1.0.0` as neither channel and hidden this release from `self-update`.

### Upgrading from v0.7.2 (Go) or from 0.8.x-beta

- **From v0.7.2 (stable channel, Go build):** this is a rewrite, so keep-at starts with fresh state - it re-scans the catalog and rebuilds its holdings. Leave the old data directory in place if you want to inspect it, but expect a first scan before seeding resumes.
- **From 0.8.x-beta:** a binary older than 1.0.0 classifies release tags by the old rule, so its `self-update` will not offer `v1.0.0` (it sees no stable tag newer than itself). Install 1.0.0 directly - re-run the install script or pass `VERSION=v1.0.0` - and later updates proceed normally. No state migration is needed; 1.0.0 reads the same files as 0.8.34-beta.
