# keep-at release notes

## v0.8.26-beta - hourly `malloc_trim`: RSS now tracks the live working set

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### Memory: hourly `malloc_trim` returns retained allocator memory to the OS

Diagnosed on a live node: a keep-at daemon seeded 165 torrents with **zero active peers**, yet resident memory sat at 2.1x its configured RAM budget (16.7 GiB against 8 GiB) and climbed ~600 MiB/h with no plateau. The memory was not live data - `/proc/<pid>/maps` showed the classic glibc malloc pattern: **165 x exactly-64-MB per-thread arena heaps (10.5 GiB retained)** and only a 147 MB main heap. On a many-core host, glibc binds each of the daemon's ~200 threads to its own arena and retains freed pages in 64 MB segments (it only ever returns top-of-heap memory), so resident memory is the *sum of every arena's historical high-water mark* and only ratchets upward as scan/check/peer bursts warm more arenas - worst case 576 arenas x 64 MB on a 72-core host. The RAM budget model anticipated ~1.9x fragmentation, but this overshoot is unbounded.

Every heartbeat now runs `malloc_trim(0)` hourly (glibc targets): the trim walks every arena and madvises free pages back to the OS, so resident memory tracks the **live working set** instead of the historical high-water mark, and the log records how much each trim returned (`malloc_trim returned ~N M of retained arena memory`). Non-glibc targets skip it. The heartbeat's existing per-minute RSS/cgroup logging makes the effect directly observable.

## v0.8.25-beta - adversarial-review fix wave: data-loss guard, follow loops, persistence, updater, socket hardening

This is a beta release for field validation; the next stable cut will be identical apart from the version tag. Every fix below was found by an adversarial review pass and pinned with a regression test before the fix landed.

### Catalog collapse guard: one bad fetch can no longer wipe the library

The removal pass deletes every held torrent (and its files) that the fresh Academic Torrents catalog no longer lists. That trusted the catalog response completely: a parse/schema accident on the catalog side (e.g. a renamed field silently skipping every row) would produce a clean, empty catalog and wipe the whole library in one scan - found by two independent reviewers. The removal pass is now refused when a fresh catalog lists fewer than 30% (default) of the held set; the scan logs a `catalog collapse guard` warning and keeps everything seeded. The threshold is tunable via `--catalog-collapse-percent` (0 disables, 100 is strictest), and docs/RECOVERY.md documents tuning plus manual recovery after a wipe (the `history.jsonl` ledger is the recovery record).

### `logs --follow` and `history` no longer stall after truncation/rotation

The follow loops used a stale byte offset: when the log cap truncated the log in place, or history rotated at 5 MB, the follower stalled until the file regrew past the old offset and then skipped everything written before it. Both now use tail -F semantics (shrink and inode-change detection) and keep streaming.

### Seed-scarcity roll: losing it really means losing it

A candidate that lost its seed-scarcity roll on the fill path could get a fresh roll per storage location through the swap path (P(admit) = 1-(1-p)^k), letting it displace strictly better-seeded held torrents with free space plentiful - against the documented design. Roll-rejected candidates are now skipped entirely.

### Privacy: read-only commands no longer chmod your home directory

`ensure_shared_dirs` walked up to `/` OR-ing 0755 into every owned ancestor, so a `--data-dir /home/alice/private/kat` turned `$HOME` itself world-traversable as a side effect of `keep-at status`. The repair is now scoped to the data dir only.

### API key survives a failed config save

`Config::save` wrote the keyless config before persisting the key file - a disk-full or permission failure in between lost the key permanently (node silently drops to anonymous). The key file is written first; a failed save is now non-destructive and retryable.

### Storage locations are canonicalized

Locations were identified by raw path spelling: symlink aliases of one directory double-budgeted it (two 110 MB budgets over one physical dir), and a config relocation orphaned state entries (accounting under-counted, free space over-reported, node re-filled on top). Locations are canonicalized and deduped at resolution; legacy state entries are re-keyed at boot.

### Self-update: channel integrity + no downgrades

The stable channel trusted GitHub's `/releases/latest` blind - a mis-shaped release published without the prerelease flag would be installed by stable users. Both channels now filter releases through the tag-shape rule, and the downgrade guard applies on the beta channel too (GitHub's release list is created_at-ordered, so a re-published older beta is "newest" by position and must not overwrite a newer binary).

### Log cap is crash-safe

The in-place cap truncated the log *before* rewriting the kept tail - a kill in that window erased the entire log, death marks included, at exactly the moment forensics matter. The kept tail is now written first and the truncate happens last.

### Terminal injection: remote titles are sanitized

Torrent titles come from any Academic Torrents registrant. ANSI escapes, OSC sequences, and newlines in a title printed raw through `hosted-torrents` and `history` - a remote attacker could rewrite your terminal title, clear the screen, or inject fake output rows. Titles are sanitized at ingest and on render.

### Swap no longer trades a live torrent for one that cannot finish

Displacement freed only *nominal* space; a sparse held torrent frees ~0 real bytes, so the swapped-in candidate could immediately hit ENOSPC and die while the displaced torrent was already gone. The swap path now gates on actual device free space after displacement (what the displaced torrents really occupied) against the candidate's full footprint.

### NaN rate limits can no longer crash-loop the daemon

A NaN `rate_limit_per_second` (YAML `.nan`) passed validation and panicked the politeness limiter on the first scrape - `panic = "abort"` in release, so the watchdog restarted into the same crash loop. Validation now rejects non-finite/non-positive values and both limiters are defensive; the census `--rate-limit` flag (which bypassed daemon validation) is validated too.

### Query socket: bounded memory and bounded connections

The query socket is world-writable, and an unbounded request read let a local user balloon the daemon's heap without limit (a unix socket sustains GB/s - the timeout bounds a stalled client, not a writing one); silent connections also pinned one fd each with no cap. Request lines are capped at 64 KiB (dropped unread) and concurrent queries at 32 (over capacity drops immediately; recovery after a flood verified).

## v0.8.24-beta - holdings history (`keep-at history`), size-capped logs, read-only config loads

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### `keep-at history`: a record of everything keep-at did to your holdings

Every addition (fill or swap), every swap's displaced set, and every drop (stalled zero-seeder eviction, vanished-from-catalog removal) is now appended to `<data_dir>/history.jsonl` and rendered by the new `keep-at history` command - tail + follow by default like `logs`, with `--all`, `--lines N`, and `--no-follow` for scripts. Additions render green, swaps (with what they displaced) yellow, drops red on a terminal, and each event carries the reason it happened, including the seed-scarcity roll statistics (chance, roll, seeder floor) that admitted the torrent. The file rotates at 5 MB (one previous generation kept), boot resume re-adds are never recorded so restarts do not pollute history, and the command works whether the daemon is running or not.

### keep-at.log is size-capped

The daemon writes its own log (stdout/stderr are redirected into the file, not streamed to journald), so nothing else rotated it and it grew without bound. It is now rewritten in place to its newest ~5 MB whenever it passes 10 MB, checked on the stats cadence. The rewrite keeps the same inode, so the redirected stdout/stderr and the crash-forensics fd stay valid, and at most a line written inside the rewrite window can be lost.

### Read-only commands no longer write starter configs

`Config::load` generates a starter config when the file is missing - a feature for `run`/`start`, but a bug for read-only paths: the `status`/`stop` daemon probe loads config paths scraped from other processes' `/proc` cmdlines and could materialize a starter at a path owned by a concurrently-starting daemon (observed as a rare race; on real hosts it could create unexpected files). All read-only commands (status, stop, logs, history, hosted-torrents, network-status, triage-last-exit) now load configs without ever writing; only `run`/`start`/`service install` generate starters. `status --config <missing>` now reports `no config at <path>` instead of writing one. Pinned by a regression test.

### Release pipeline hardening

GitHub Actions are pinned to full commit SHAs (mutable tags could be repointed upstream of your build), the release tag reaches the publish script via an env var instead of textual workflow interpolation (a hostile tag name can no longer execute shell in the runner), and the Docker image now runs as an unprivileged user (uid 1000) - bind-mounted `./data` and `./storage` must be writable by that uid on the host (see README); named volumes are seeded correctly.

## v0.8.23-beta - SIGPIPE no longer fatal (death-mark fix)

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### The kill forensics themselves could kill the daemon on a broken pipe

Since v0.8.15 the death-mark system caught SIGPIPE along with the truly fatal signals — but its handler logs the mark and then re-raises under the default disposition, killing the process. That overrode the daemon's own `SIG_IGN` posture (Rust runtime default, re-pinned at startup), converting a routine broken-pipe write into a daemon death: observed live, a `status` client that disconnected mid-response (large booting held-list, client Ctrl-C/time-out/disconnect) made the daemon's socket write raise SIGPIPE and the daemon died right after boot. SIGPIPE is now excluded from the death-mark signal list: the daemon keeps `SIG_IGN`, broken-pipe writes return `EPIPE`, the socket server logs the failure at debug and keeps serving. Death marks remain for the genuinely fatal catchable signals (SIGSEGV/SIGABRT/SIGBUS/SIGXCPU/SIGHUP/SIGQUIT). Pinned by a regression test; the e2e signal-disposition behavior was verified by A/B harness (pre-fix dies on a direct SIGPIPE, fixed survives and keeps serving).

## v0.8.22-beta - status disk line on `limit: max` nodes

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### Disk line no longer vanishes on `--storage-limit max` nodes

The live query's storage vec was built from the config *before* `Engine::new` resolved `limit: max` to concrete bytes, and `limit_bytes()` of an unresolved `max` is 0 — so `status` gated the whole disk line out, on `max` nodes in every phase (booting and seeding alike); only the offline snapshot fallback showed it. Limits now resolve before the live query binds, so the disk line always shows, e.g. `disk: 1.2 TiB used (61.5%), 1.9 TiB committed (100.0%), 1.9 TiB configured`. Belt and braces: if a zero limit ever reaches it anyway, the line reads `disk: statistics unavailable — will appear once startup completes` instead of being silently absent. Pinned by a regression test; nodes with explicit byte limits are unaffected.

## v0.8.21-beta - bounded file-handle pool (EMFILE wedge fix), reorganized disk line

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### Disk line reads used → committed → configured

`status` now orders the disk line figure-by-figure, each with its own percent of the configured total: `disk: 2.5 TiB used (66.6%), 3.8 TiB committed (100.0%), 3.8 TiB configured`. Previously configured sat in the middle of the used clause, which read as a single comparison; the committed segment still appears only when nonzero (older snapshots without committed tracking omit it).

### File handles are pooled, not one-per-library-file

Stock rqbit opens every file of every torrent in read/write mode at add time and keeps every fd open until removal - one fd per library file, forever. On file-heavy libraries that exhausts the process fd limit and wedges the whole daemon: `too many open files (os error 24)` on everything, dead TCP listener, dead status. A single 29k-file dataset held 44% of a 65,536-fd ceiling on its own. keep-at now installs a bounded LRU file-handle pool through rqbit's public storage-factory hook (the same design libtorrent's `file_pool_size` and Transmission's `tr_open_files` have used for decades): `init` only creates 0-byte files, and every read/write gets its handle from a process-wide pool capped at `min(4096, hard_limit - 2048)` open fds. Position-based IO shares one fd across threads with no per-file locking, in-flight IO survives eviction, and pause/resume is free. A transient fd spike sheds a quarter of the cache and retries; persistent exhaustion surfaces as an orderly per-torrent error instead of a daemon-wide freeze. Pool sizing happens after the rlimit raise and is overridable with `KEEPAT_FD_POOL_CAP` for tuning.

## v0.8.20-beta - disk used now reports allocated bytes, not apparent size

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### `used` no longer counts sparse holes, so it matches what the host charges

keep-at writes sparse files (rqbit truncates a torrent's files to full length and fills pieces in as they download), and the disk walk summed *apparent* file sizes - so a partially-downloaded library reported its full eventual footprint as "used", making `used` identical to `committed` and hiding real quota consumption. Observed on a 3.8 TiB node: status claimed 3.8 TiB used while the host's quota meter showed 2.78 TB. `dir_size_bytes` now sums allocated blocks (`st_blocks` × 512, what `du` and every quota meter report), so the disk line finally shows both truths: `2.53 TiB used (66%)` of what is physically on disk next to `3.8 TiB committed (100%)` of reservation, with the gap being data still to materialize. The seeding heuristic in `hosted-torrents` and the download-completion checks are unaffected (a fully-downloaded torrent's allocation covers its nominal size). Pinned by a sparse-file regression test.

## v0.8.19-beta - committed storage tracking in status

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### `status` shows storage committed vs storage used

The disk line previously showed only actual on-disk usage, which undercounts what the node has reserved: while torrents are being integrity-checked or downloaded their files don't fully exist yet, so a freshly-committed library reported near-0 used with no way to see that the storage was already spoken for. `status` now shows both figures - `disk: 45.0 GiB used of 100.0 GiB configured (45.0%), 50.0 GiB committed (50.0%)` - where committed is the sum of held torrents' nominal sizes, the reservation basis the scanner's free-space decisions budget against (plus a small fixed 256 KiB buffer per torrent). During a boot, committed is exact immediately (from state.json) while used catches up as checks and downloads materialize data on disk; the same numbers flow into the `runtime stats` log line (`committed=...`) and the persisted snapshot, so the offline fallback shows them too.

## v0.8.18-beta - booting state in status, listener panic fix, disk-usage cache

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### Daemon death (SIGABRT) during large-library boots is fixed

Field validation on a 4TB host reproduced a core dump during boot: rqbit's listener task selects between accepting connections (disabled once pending handshake checks reach a cap) and handshake completion pattern-matched as `Some(Ok(..))` - and on a large-library boot, torrents sit in integrity checks past the 5s handshake live-wait, so incoming peers' checks fail en masse; the first failure while the queue is at cap disables every branch and tokio panics. With `panic = "abort"` that killed the whole daemon, and the watchdog restart could re-enter the same window (crash loop). keep-at now raises the pending-handshake cap to unbounded (`listener_options` in `src/engine/session.rs`), which keeps the accept branch permanently enabled so the panic is unreachable; the trade-off and upstream status are documented in docs/DESIGN.md. Pinned by a unit test on the constructed listener options.

### `status` says "booting" while the daemon starts up

During startup the daemon already answered queries with a booting view, but the headline line still read "keep-at is running", which read as ready when the engine was minutes away from serving. `status` now prints `keep-at is booting (pid N)` for that state, the header shows how long it has been booting (`booting for 3m 12s`), and the state line explains what startup does: resuming the held library, verifying existing data, seeding beginning as checks complete.

### Disk usage is cached for live queries, ending boot-window unavailability on huge libraries

Every `status` call made the daemon recursively walk every storage location; on a 4TB library under boot-time hashing load that walk could exceed the 5s query timeout, so the booting window reported `(live stats unavailable)` and fell back to the snapshot even though the daemon was healthy. The disk sum is now cached for 60s and seeded at boot from the last persisted snapshot, so the first query after boot is instant and subsequent queries reuse it - freshness is unchanged from what the offline fallback would have shown anyway (60s stats cadence).

## v0.8.17-beta - flaky live status fixed, query failures logged

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

### `status` flakiness on CPU-constrained hosts is fixed

Field validation on a CPU-constrained host showed roughly half of `status` invocations printing `(live stats unavailable)` against a demonstrably running daemon, failing instantly (~0.1s, not after the 5s query timeout), with no pattern. Root cause: the daemon served accepted socket connections through a std-socket conversion that left the fd in O_NONBLOCK mode, so when a client connected but its request bytes had not yet arrived (a routine preemption on a loaded host), the first read returned EAGAIN instead of waiting and the connection was dropped immediately — the client saw a broken pipe or EOF and fell back to the snapshot. The server side now uses proper async I/O with the same 5s timeout: a late request is waited for and served, not raced. A regression test pins it by connecting, delaying the request by 300ms, and asserting the response arrives. As part of the same diagnosis the per-query socket close is also less surprising: write and read failures inside the daemon are logged (at debug) instead of vanishing, and the read timeout now also covers the response write.

### Live-query failures are now visible in the daemon log

The query client previously collapsed every failure mode — connect refused, write failed, connection closed without an answer, unparseable response — into a silent fallback to snapshot files, which made the flakiness above take three debugging rounds to localize. Absent/refused connections (the normal not-running case) remain silent; anything else is logged as a warning naming the socket and the failure. A running daemon that accepts a connection but never answers is now immediately diagnosable from `keep-at.log` alone.

### Live query socket survives bind races at boot

The socket bind used to be a single attempt: a lost race with a previous daemon generation's socket file (or an unlucky scheduling window) disabled live queries for the entire daemon lifetime, leaving only a one-line warning. Bind now retries for 30 seconds at 500ms intervals, re-probing and unlinking an unowned socket between attempts, before giving up and falling back to files.

### `status` wording trimmed

The `(live stats unavailable)` and integrity-check state lines are shortened; the parenthetical explanations they carried moved here and to the docs.
