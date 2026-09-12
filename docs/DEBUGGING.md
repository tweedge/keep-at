# Debugging keep-at

Production diagnostics that ship in every build, plus the one gated test
knob. Nothing here needs special compilation.

## Always-on diagnostics

**Death marks.** Handlers for SIGSEGV, SIGABRT, SIGBUS, SIGXCPU, SIGHUP,
SIGQUIT, and SIGPIPE write a one-line mark to the log file (async-signal-safe
write on a pre-opened append fd) before dying. SIGKILL cannot be caught — a
silent death with no mark and a stopped heartbeat is itself the diagnostic
(external SIGKILL). SIGPIPE additionally cannot kill the daemon in the first
place (see below); the mark exists as defense-in-depth.

**Heartbeat.** Every 60 s the daemon samples RSS, open-fd count, and cgroup
memory state (current/peak/oom_kill) and atomically replaces
`<data-dir>/heartbeat.json` (256 bytes, overwritten in place — never grows).
A matching line lands in the log. A stale heartbeat with no death mark means
external SIGKILL; a `death-mark:` line means the named signal.

**Panic hook.** Panics write `death-mark: PANIC at <file:line>: <msg>` to the
log before the abort. Combined with `panic = "abort"` in release builds, this
means every crash class is visible in the log.

**stdio redirect.** In daemon mode stdout/stderr are dup2'd into
`<data-dir>/keep-at.log`, so the process holds no pipe or journald fds and no
write can ever hit a closed reader (the historical silent-SIGPIPE death —
fixed in 0.8.15). Panic output lands in the log file.

## Post-mortem

```
keep-at triage-last-exit --data-dir <dir>
```

Prints the surviving cgroup memory state (which outlives the process) and the
last heartbeat. Intended for a host watchdog to run when it finds the daemon
dead, before restarting — see `scripts/watchdog.sh` for a reference
implementation with the capture wired in.

## Gated test knob

```
KEEPAT_DEBUG_PANIC=1 keep-at run ...
```

Schedules an intentional panic in a background thread 20 s after boot, to
measure death handling on the real binary (the panic, the hook text, and the
subsequent abort must all land in the log). Gated to the exact value `1` and
self-announcing with a WARN line when armed. Never set it outside deliberate
testing.

## Historical note

The 0.8.11–0.8.14 releases died silently, roughly daily, on field hosts:
`main()` restored SIGPIPE to SIG_DFL (for CLI `| head` behavior) for every
arm including the daemon, so any write to a broken pipe or a closed journald
stream killed the process mid-write. Rust socket writes are immune
(MSG_NOSIGNAL), but stdout/stderr under systemd are journald stream sockets,
and v0.8.14 shipped no log file — every log line was a tripwire. 0.8.15
scoped the SIGPIPE restore to print-style CLI arms, pinned the daemon to
SIG_IGN, and dup2'd stdio into the log file.
