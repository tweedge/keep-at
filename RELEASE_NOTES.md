# keep-at release notes

## v0.8.29-beta - fix: AT seeding attribution on flag-only installs

### Seeding attribution: the `<data_dir>/api_key` file was ignored by flag-only runs

A daemon started purely from CLI flags (`--storage-location ... --max-ram 8G`, no `--config`) never read the API-key file, so `userannounce` was never resolved and every tracker announce went out unkeyed: Academic Torrents listed the node as "public" in a torrent's mirrors table instead of the operator's account, even with the key correctly sitting in `<data_dir>/api_key`. Only daemons launched with `--config` picked the key up. Found by live swarm-table testing (a keyed test announce attributed instantly; the same host's daemon peers showed "public").

The key file is now merged into every resolved config (`Config::merge_key_file`, called from both the file-load path and flag resolution after the data dir settles). Precedence: an explicit `--api-key` flag beats the file, which beats nothing. After installing or changing `<data_dir>/api_key`, restart the daemon once — attribution applies from the next announce; the tracker's own min-interval (30 min on AT) governs when each torrent re-announces.

## v0.8.28-beta - DHT disabled: eliminates the memory leak root cause found by production heap profiling

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### Memory: DHT is now permanently off

This release turns the DHT subsystem off in the daemon's session configuration, closing the memory leak root cause identified by 40+ hours of production heap profiling on the v0.8.27 line (full evidence in notes/PROD-TEST-LOG.md, 2026-09-21T08:40Z). The profile attributed the entire steady-state leak to the per-torrent DHT lookup loops: `request_peers_forever` (v4+v6, spawned per torrent) accumulates the per-request tokio task cells inside its `FuturesUnordered` indefinitely - 944 bytes each, at a net rate of ~28-38/s scaling with DHT network activity and live torrent count - i.e. ~129-235 MiB/h at 181 torrents, with no plateau. The growth is invisible during downloads (buried under churn) and only shows in a steady seeding regime, which is this daemon's normal state.

The daemon no longer participates in the DHT: no announce, no peer discovery via DHT, no routing table persistence. This is intentional. Mercury's swarms are tracker-centric (Academic Torrents), and with DHT off, seeding and peer service continue unchanged through the trackers - measured over the validation window, upload and on-demand download traffic continued normally (up 2.7 GiB, down 20.9 GiB while memory sat flat). Memory behavior with DHT off: resident memory is flat at ~550 MiB with 183 torrents held (vs +129-235 MiB/h of growth with DHT at the same workload); the live working set is ~113 MiB allocated (~0.4 MiB/torrent + ~40 MiB base) and does not grow.

Re-enabling DHT requires an upstream librqbit-dht fix for the FuturesUnordered accumulation; the heap-profile evidence and reproduction recipe are documented in the repository notes for the upstream report. If DHT support is ever restored, the leak returns with it.

### Operational notes

The jemalloc allocator swap from v0.8.27-beta is unchanged and remains the second half of the memory story (no arena ratchet, no sawtooth). Deployment expectation on monitored hosts: the `malloc_trim` lines were already gone in 0.8.27; the `rss=` heartbeat line remains the memory signal, and it should now track ~550 MiB flat on an 8 GiB-budget node at ~180 held torrents, independent of DHT or swarm activity.