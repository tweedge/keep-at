# keep-at release notes

## v0.8.31-beta - selection-gate and library-safety fixes: catalog-collapse guard, one scarcity roll per candidate, eviction order

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

This release is a code-quality pass over the selection, eviction, and persistence core: no new features, but several places where the code did not do what its own documentation promised, including one that could delete a healthy library.

### The catalog-collapse guard now checks what it says it checks

The guard that stands between a bad catalog fetch and deleting your library was comparing the *total item count* of the fetched catalog against the number of torrents held - so a well-formed catalog of entirely the wrong entries (a schema accident, a redirect to a different feed, RECOVERY.md's "perfectly-formed catalog with wrong rows" case) sailed straight past it and the removed-from-catalog pass then deleted the held torrents **and their downloaded data**.

The guard now measures the overlap it was always documented to measure: what fraction of the held set the fresh catalog still lists. A same-volume catalog sharing no hashes with your library is refused exactly like a truncated one. This is the failure mode docs/RECOVERY.md describes; with `vanished_eviction_timeout: 0` (which disables the grace window) the old behavior wiped the library on the very next scan.

### One seed-scarcity roll per candidate, not one per storage location

The seed-scarcity gate is the polite-admission mechanism that keeps keep-at from piling onto already-healthy swarms. A candidate is supposed to get exactly one independent chance per scan (DESIGN.md). The swap path drew a fresh roll for every storage location it tried, so on a multi-location node - and on any node whose fill path was skipped because the torrent cap was reached, the disk was full, or the RAM budget was exhausted - a candidate got `1-(1-p)^k` chances to displace a *better-seeded* held torrent instead of the documented `p`. The roll is now drawn once per evaluation and shared by both placement paths.

### Well-seeded swarms could be admitted with certainty

`complete` in a tracker scrape parses to any 32-bit number, and the seed-scarcity exponent was being narrowed to a signed 32-bit integer for the power function. A reported seeder count past 2^31 wrapped the exponent negative and turned `aggressiveness^n` into infinity - i.e. "always admit this swarm", the exact inversion of the gate's purpose. The exponent is clamped now, and the resulting `chance` is always finite.

Worse, a non-finite `chance` made the history event unserializable, and the ledger **silently dropped** the record - so the poisoned add left no trace in `keep-at history`. History now logs loudly if an event ever fails to serialize instead of vanishing.

### Zero-bias eviction dropped the most urgent swarm first

With no size bias to order by (a location whose total size couldn't be read), eviction fell back to seeding order - and the ordering was inverted, evicting the *fewest*-seeded qualifying torrent first. That removes keep-at's support from the swarms that need it most, and is the reverse of both the documented legacy order and the ranking it is supposed to mirror. Fixed, with the pending ATTACK C regression test that had been noted but never written.

### Config files now reject bad values the way flags do

`aggressiveness: 0`, `scan.rate_limit_per_second: 0`, and `port: 0` in a YAML config were silently rewritten to their defaults before validation, while the identical values on the command line were correctly refused. An operator writing `aggressiveness: 0` meaning "never auto-select anything" got a daemon quietly running at 0.6 and downloading. Omitted fields still get their defaults; explicit zeros now fail validation like every other entry point.

The `network-status` rate limit also now gets the same floor as the daemon (a rate of one request per ~31 years used to be accepted and would wedge the probe), and its `.torrent` fetches are throttled like its scrapes - they used to go out in an unpolite burst ahead of a rate-limited scrape.

### Operational fixes

- A swap that displaced more torrents than it added left the in-scan fill gate counting too high, so later candidates in the same scan refused free space and walked the swap path until the next scan re-derived the count from state. The count now follows the real held set.
- `network-status`'s node count mangled IPv6 peers: addresses were truncated at the first colon, collapsing every IPv6 peer sharing a first hextet into one bogus "node". The number is now a real distinct-IP count on IPv6 swarms.
- The "AT-only" tracker filter matched on a substring, so a lookalike host such as `evilacademictorrents.com.attacker.net` was kept in the announce list - held torrents would have announced to it - while legitimate third-party trackers were dropped. It now matches the exact host or a real subdomain. The API key was never at risk: it is only ever attached to an exact https `academictorrents.com` URL.
- The self-update downgrade guard is now tested against the code that ships. The integration test used to replicate `cmd_self_update`'s decision inline, so it stayed green no matter what happened to the real guard; the decision lives in `updater::decide` and both call it.
- `keep-at` processes installed under a renamed binary (e.g. `my-keep-at`) were found by the process scan and then immediately called dead by the PID-file liveness check, because the two used different argv0 rules. One predicate now answers both.

### Performance

state.json was being fully rewritten and fsynced once per held torrent by two loops that run every scan - catalog confirmation and seeder refresh - which is up to twice the held count in full-file writes per scan (hundreds of rewrites and fsyncs per scan on a Pi/SD node with a large library). Both now apply the whole pass in a single write, matching what the watchdog progress pass already did.

### Corrected documentation

docs/DESIGN.md claimed the seeder session runs with DHT on; it has been off since a production heap leak was traced to the DHT subsystem (`notes/PROD-TEST-LOG.md`, 2026-09-21), and session.rs still carried a stale "DHT on" comment above the code that disables it. The stall-eviction rule was documented as "zero seeders AND no progress" when the code deliberately applies to any incomplete torrent regardless of swarm depth. The file-descriptor comments still described stock rqbit's one-fd-per-file storage, which `pool_storage` replaced with a bounded handle pool - the many-file admission guard is a documented conservative upper bound against that newer model, not an exact price.
