# keep-at release notes

## v0.8.27-beta - allocator swap: jemalloc replaces glibc malloc, ending the RSS sawtooth

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### Memory: freed pages now return to the OS continuously

The hourly `malloc_trim(0)` workaround introduced in v0.8.26-beta is replaced by a structural fix: the daemon now links **jemalloc** as its allocator on Linux gnu targets (covering every released binary and the Docker image; other targets keep the system allocator). glibc malloc retains freed pages in per-thread 64 MB arena segments and only ever returns top-of-heap memory to the OS, so resident memory ratchets toward the historical high-water mark, and the hourly trim produced a sawtooth: freed pages accumulating in arenas for up to an hour between trims before being handed back in one lump. jemalloc instead returns freed pages continuously - dirty pages decay back to the OS over ~10 seconds (sigmoidal decay curve), purged proactively by a background thread that is enabled at boot and verified by read-back - so resident memory tracks the live working set with no accumulation window and no trim call at all.

Two observables change. The hourly `malloc_trim returned ~N M of retained arena memory` info line is gone by design; the boot log now carries `allocator: jemalloc (background purging on, dirty decay 10000ms)` as the health signal (a WARN variant, `background purging NOT running`, is actionable - check `MALLOC_CONF`), and the heartbeat's `rss=` line remains the steady-state signal. `MALLOC_CONF` environment tuning (decay times, arena count) is honored by the daemon from process start; `MALLOC_ARENA_MAX` is inert under jemalloc. The direct `libc` dependency is dropped and `tikv-jemallocator`/`tikv-jemalloc-ctl` 0.7 are added as target-gated dependencies.

### Expected: RSS still climbs on nodes with heavy peer churn - that is the upstream rqbit leak, not an allocator regression

The rqbit peers-map leak diagnosed alongside the v0.8.26-beta ratchet (rqbit keeps an entry for every peer ever connected in a per-torrent map - issue #525, fix pending upstream) is *referenced* memory, which no allocator can return. A node observed growing at ~210-600 MiB/h under 0.8.26-beta will keep growing the same way under 0.8.27-beta; what changes is that the growth now sits exactly at the leak rate with no arena retention or hourly sawtooth on top, and it cannot ratchet beyond the live set. The upstream fix is being prepared as a PR to rqbit; once it ships and a release containing it lands here, resident memory should settle at the live working set and stay there without restarts or trims.

### Tests and tooling

`tests/catalog_collapse.rs` no longer runs its two engine tests concurrently: both raced on rqbit's shared persisted-DHT state (the same `~/.cache/com.rqbit.dht/dht.json` and persisted UDP port), flaking roughly 1 in 5 full-suite runs - the file now serializes them. `scripts/build-release.sh` documents and pre-checks `make` (the vendored jemalloc inside tikv-jemalloc-sys builds with configure+make on every target; autoconf is not required), and docs/DEBUGGING.md documents the new allocator diagnostic line.
