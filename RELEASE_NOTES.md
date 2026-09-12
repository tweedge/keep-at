# keep-at release notes

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
