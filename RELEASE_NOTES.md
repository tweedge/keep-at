# keep-at release notes

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
