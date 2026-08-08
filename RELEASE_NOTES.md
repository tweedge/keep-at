## v0.4.0 - proper client identity in the swarm

Until now, keep-at identified itself correctly to other peers (the BEP 10 extended handshake "v" string was already `keep-at/<version>`), but the HTTP User-Agent it sent on tracker announces was left at the torrent library's default - so Academic Torrents' tracker recorded keep-at peers as `anacrolix-torrent`, and its Technical page showed that instead of keep-at. This release fixes the whole identity story, and distinguishes keep-at's two roles so a node that's merely probing a swarm never gets mistaken for one that's actually seeding.

- **Tracker announces now identify as keep-at.** The main (seeding) client sends `keep-at/<version> (seeder)` as its HTTP User-Agent on tracker announces, which is what AT's tracker records and displays on each torrent's Technical page. keep-at seeders now show up as keep-at, not anacrolix-torrent.
- **Two roles, visibly different.** The probe (scraper) client - used only to briefly join swarms and count other keep-at nodes during a scan - sends `keep-at/<version> (scraper)` in both its User-Agent and extended-handshake "v" string, so AT's logs and other keep-at nodes can tell a prober from a seeder. keep-at's own HTTP tracker scrape requests carry the scraper identity too.
- **Anti-cascade and network-status count only seeders.** `keep-at network-status` and the anti-cascade peer count now ignore any peer whose client string marks it as a scraper (older keep-at versions advertising the bare `keep-at/<version>` string still count as seeders). A node that's only probing a swarm no longer inflates how many keep-at nodes appear to be seeding a given torrent.

---

## v0.3.5 - dependency security fixes

This release upgrades the indirect dependencies behind all 20 GitHub Dependabot advisories (7 critical, 3 high, 10 moderate) that were open on the default branch. keep-at's own code was not at fault - the advisories were in transitive dependencies - but the versions have been bumped to patched releases:

- `golang.org/x/crypto` v0.44.0 -> v0.53.0 (fixed the SSH/FIDO key-constraint, `@revoked` bypass, channel deadlock, and pathological-input panics)
- `golang.org/x/net` v0.47.0 -> v0.56.0 (HTML parser and DNS SVCB/HTTPS panic DoS)
- `golang.org/x/text` v0.31.0 -> v0.39.0, `golang.org/x/sys` v0.45.0 -> v0.46.0, `golang.org/x/sync` v0.20.0 -> v0.21.0 (cascaded upgrades)
- `go.opentelemetry.io/otel` v1.38.0 -> v1.42.0 (baggage-header allocation DoS amplification)
- `github.com/pion/dtls/v3` v3.0.3 -> v3.1.5 and `github.com/pion/stun/v3` v3.0.0 -> v3.1.5 (panic DoS on crafted messages; DTLS nonce/key-leak)

`github.com/anacrolix/torrent` remains **pinned at v1.60.0** - v1.61.0 has a crash bug keep-at must not take (see DESIGN.md) - so these are upgrades of its transitive dependencies only, not the library itself.

Verified with `govulncheck`: zero vulnerabilities reachable from keep-at's code. The one remaining advisory (the unmaintained `golang.org/x/crypto/openpgp` package) has no upstream fix and is not called by any of keep-at's code paths. All 7 release targets build, and the full test suite passes.

---

## v0.3.4 - API key attribution

keep-at can now attribute the torrents it seeds to your Academic Torrents account, so the details page shows your name and image as hosting the data. Add your API key (from https://academictorrents.com/my.php) via `--api-key` or `api_key` in the config file.

At startup, keep-at resolves the key through AT's own `userannounce` endpoint (the same mechanism AT's smartnode tooling uses) into the per-user announce URL carrying your account's passkey, then uses that URL for every announce to AT's trackers.

- The key is **only ever sent to the two Academic Torrents tracker hosts** (`academictorrents.com` and `ipv6.academictorrents.com`, https only). Third-party trackers in `.torrent` files never see it.
- The key and the resolved passkey URL are **never logged, never written to cached .torrent files or state, and never surfaced anywhere** except the announce to AT's tracker.
- If the key is invalid or the endpoint is unreachable, keep-at logs a warning (with the secret portion redacted) and keeps running unattributed - it never crashes or refuses to start.

Verified live against Academic Torrents: with a key configured, keep-at seeded real torrents and the account attribution was confirmed on the site.

---

## v0.3.3 - bugfix

Fixed the 0.3.2 release build: the new RAM-aware `SystemTotalRAM` used a Linux-only syscall, which broke cross-compiling the macOS and Windows binaries that the release workflow ships. It now uses the right per-platform API (Linux `sysinfo`, macOS `hw.memsize`, Windows `GlobalMemoryStatusEx`), so `scripts/build-release.sh` succeeds for every target. On a platform where RAM can't be measured, keep-at logs that the RAM-driven torrent cap is disabled rather than refusing to hold anything.

---

keep-at now bounds its own RAM use and scales how many (and which) torrents it holds to fit the machine it's running on - so a tiny 1 GB Raspberry Pi on a big disk seeds just as sensibly as a server with 32 GB. This release also makes scans dramatically faster and start seeding sooner.

## New: faster scans, seeding starts immediately

The initial full-catalog scrape used to take hours and left configured storage idle the whole time. Three changes fix that:

- **Scrape results are cached across scans.** Each torrent's seeder/leecher counts are remembered on disk (TTL = the scan interval). A weekly re-scan reuses last week's counts instead of re-querying Academic Torrents' tracker for every catalog item, so repeat scans cost almost nothing beyond the torrents keep-at actually chooses to seed.
- **Swarm probing moved to decision time.** The anti-cascade probe (which waits several seconds per candidate to count other keep-at nodes) used to run for every available candidate during evaluation. It now runs only for the candidates keep-at is about to act on, so probe time drops from "every available candidate" to "everything it decides to seed."
- **Incremental acting.** Candidates stream out of evaluation as they complete, and keep-at seeds the highest-priority ones right away - the top of the running ranking fills as soon as it's known, instead of after the whole catalog is walked. It never seeds something that isn't genuinely among the best it could hold (the acted window is capped at the RAM-derived torrent limit), so the "seed the most urgent torrents" guarantee still holds.

Net effect: the most urgent torrents start seeding within minutes of a scan starting, and repeat scans are nearly free.

## New: RAM-aware holding

- Added a `--max-ram` flag (and `max_ram` config key) for the most memory keep-at will plan around. It defaults to **80% of system RAM**, and an explicit value is never allowed to exceed that hard cap (keep-at refuses to start rather than overcommit the host).
- RAM scales with the **number** of torrents keep-at holds, not their total size: the underlying BitTorrent library gives every held torrent its own independent pool of peer-connection buffers. So `max_ram` translates into a hard cap on the torrent count, logged at startup as `max_torrents`. A free-space fill that would exceed the cap is skipped; only swaps (net-zero or negative count) proceed.
- When RAM is the binding constraint rather than disk, the decision function flips to **prefer larger torrents**: more bytes get seeded per scarce RAM slot, and eviction sheds the smallest held torrents first so big ones survive. This is exactly the "prioritize larger torrents to fill space when RAM runs out before disk" behavior - a 1 GB Pi on a 20 TB disk holds fewer but bigger torrents and stays effective.

## Changed: lower default connection footprint

keep-at is a seed-first, always-on daemon that assumes many keep-at nodes share the Academic Torrents seeding load, so no single node needs to be a speed monster. The default per-torrent peer fanout and buffer sizes are now much smaller (12 established connections per torrent instead of 50, 256 KiB per-connection buffer instead of 1 MiB). This cuts the dominant RAM cost by roughly 4x and is what makes the RAM budget meaningful on small boxes.

## Planning notes

- The per-torrent footprint estimate (`EstablishedConnsPerTorrent × MaxAllocPeerRequestDataPerConn + a fixed base`) is a deliberate conservative upper bound used only for planning the torrent-count cap, not a precise live allocation accounting.
- `max_ram` is honored from both `--max-ram` and a config file's `max_ram`; the 80%-of-system hard cap applies to both.

Real full-catalog testing surfaced crashes and scaling problems that a small hand-picked test never hit. This release fixes all of them and adds visibility into how long a scrape is actually taking.

## Fixes

- **Crash fix**: pinned `github.com/anacrolix/torrent` to v1.60.0. v1.61.0's rewritten tracker announce dispatcher has a locking bug that surfaces as an unrecoverable Go runtime fatal error ("sync: Unlock of unlocked RWMutex"), triggered from more than one code path under the torrent churn a real scan produces.
- Swarm probing now runs on its own disposable, DHT-disabled torrent client instead of adding and dropping torrents on the main one - that add/drop churn was an independent trigger for bugs in the same family, regardless of library version. DHT stays enabled on the main client used for real downloads, since disabling it globally measurably hurt download connectivity.
- Candidates are now evaluated concurrently (16 at a time). Swarm probing alone can take several seconds per candidate; run sequentially across a catalog with thousands of them, a scan would take hours longer than it needs to.
- The torrent client's automatic tracker re-announcing - separate from keep-at's own rate-limited scrapes - was enough on its own to get keep-at rate-limited (HTTP 429) by Academic Torrents in real testing. It now shares the same rate limit.
- Replaced the panicking `TorrentSpecFromMetaInfo` with the non-panicking `TorrentSpecFromMetaInfoErr` throughout: a decade-plus of inconsistent uploads means a malformed `.torrent` file is an expected case, not an exceptional one.
- Added a recover()-based safety net around per-candidate work, as defense in depth against the (much larger) class of ordinary panics that inconsistent metadata can trigger.
- Quieted an overwhelming amount of expected log noise (dead third-party trackers, malformed webseed URLs) from the underlying torrent library, without touching keep-at's own logging.

Verified with a real, unbounded run against the entire live Academic Torrents catalog (2,850 items) capped at 10GB in /tmp: stable for 20+ minutes and 200+ candidates processed with zero crashes, versus crashing within about a minute before this release.

## New: scrape progress logging

A full-catalog scrape can run for a long time, and previously the log went quiet for the whole thing. Three log lines now mark the phase:

- `"starting scrape"` once, up front, stating explicitly that it can take a while and that keep-at won't add, swap, or remove anything it holds until the scrape finishes.
- `"scrape in progress"` every 2 minutes while it runs, with percent complete and a human-readable ETA (e.g. `2h15m`, `5m30s`).
- `"scrape complete, updating what keep-at holds"` once it finishes, right before keep-at acts on the results.

See docs/DESIGN.md for the full writeup of all of the above, under "Running against the real catalog" and "Reporting progress during a long scrape."
