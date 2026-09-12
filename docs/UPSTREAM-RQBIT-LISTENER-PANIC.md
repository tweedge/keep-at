# Upstream rqbit bug report: listener task panics when all `select!` branches are disabled

**Status as of 2026-09-12: unfixed in every published release (including 9.0.1) and in rqbit `master` (verified against upstream HEAD on 2026-09-12).**

**Where it bites:** daemon-wide `SIGABRT` during high inbound-connection activity, concentrated in boot windows where many torrents are still running their initial integrity check. Reproduced in production by keep-at on a 4 TB host (details in "Evidence" below). Downstream users cannot work around it safely at runtime — the mitigation is a configuration raise (see "Suggested fixes"), but a real fix belongs in rqbit.

---

## Summary

`Session::task_listener` (crates/librqbit/src/session.rs, the TCP listener loop; same shape in the µTP listener) runs this `tokio::select!` forever:

```rust
loop {
    tokio::select! {
        r = l.accept(), if futs.len() < max_pending_incoming_handshake_checks => {
            match r {
                Ok((addr, (read, write))) => {
                    ...
                    futs.push(
                        session.check_incoming_connection(addr, A::KIND, Box::new(read), Box::new(write))
                            .map_err(|e| { debug!("error checking incoming connection: {e:#}"); e })
                            ...
                    );
                }
                Err(e) => { warn!(...); sleep(10s); continue }
            }
        },
        Some(Ok((live, checked))) = futs.next(), if !futs.is_empty() => {
            ...
        },
    }
}
```

There are two ways for **both branches to be disabled simultaneously**, and in tokio a `select!` whose branches are all disabled panics with *"all branches are disabled and there is no else branch"*:

1. **The accept branch's guard is true-and-keepable.** `futs.len() < max_pending_incoming_handshake_checks` (default 256, `DEFAULT_MAX_PENDING_INCOMING_HANDSHAKE_CHECKS` in `librqbit/src/listen.rs`) disables accept while the pending queue is full.
2. **The `futs.next()` branch disables itself on failure.** Each pending future is a `check_incoming_connection` future whose output is `anyhow::Result<...>`. The branch pattern is `Some(Ok((live, checked)))` — an `Ok` **or** a `None` (queue drained) fails the pattern and disables the branch. A failed handshake check (timeout, torrent not live, read error, peer misbehaving) therefore does not pop the queue in this branch; it returns `Err(..)`, which fails the pattern.

So the panic triggers the instant two conditions coexist at a poll point:

- `futs.len() >= max_pending_incoming_handshake_checks` (accept disabled), **and**
- the head future of `futs` resolves to `Err(..)` (the `Some(Ok(..))` pattern fails).

## Why the failure window is real and reachable

`check_incoming_connection` calls `torrent.live_wait_initializing(Duration::from_secs(5))` and errors out if the torrent is still in the `Initializing` state (piece-hash integrity check) after the wait. Clients that see a full library mid-boot hit exactly this: they connect to a seeding port, get checked against torrents that are still hashing, and the check fails after 5s. One such failure *while the pending queue sits at the cap* arms the panic on the next `select!` evaluation.

The cap was presumably meant to bound memory under handshake floods; but because failures do not pop the queue **in the branch that succeeds**, the loop loses its only mechanism to drain the queue when accept is disabled — the design assumes successes dominate, and the panic is the failure path that proves otherwise.

A secondary aggravator: it is possible (e.g. under load, or when the accept-error path `sleep(10s)` lands) for the loop to reach a state where accept is disabled, the queue is non-empty, and the only pending future resolves with `Err` — the panic is then deterministic, not merely probabilistic.

## Impact when it fires

- Inside a tokio worker, a panic aborts the task; but keep-at (like many embedders) compiles with `panic = "abort"`, so the **whole process dies with SIGABRT**.
- Any process supervisor that restarts the binary after a crash re-enters the same window on the next boot (large library still hashing + the swarm remembers your address and reconnects quickly), producing a **crash loop** — observed in the field.
- Because the death is a panic at abort, log output beyond the death-mark is lost unless the embedder installs a panic hook. (keep-at does, which is how this was captured.)

## Evidence from the field (keep-at on Academic Torrents, 2026-09-12)

- Host: 4 TB library, thousands of held pieces, watchdog-managed daemon.
- Symptom: daemon died with SIGABRT during boot; log contained:
  `death-mark: PANIC at .../librqbit-9.0.1/src/session.rs:980: all branches are disabled and there is no else branch`
  (line 980 is the `tokio::select!` opening in `task_listener`; the µTP listener shares the shape.)
- Correlated with the boot window, where dozens/hundreds of torrents were in rqbit's `Initializing` state and inbound peers were being rejected by `live_wait_initializing`'s 5s timeout.
- Not fixable from the embedder side by reducing load alone: the failure is timing-dependent but reachable whenever (a) the library is large enough that boot hashing outlives the 5s handshake wait, and (b) the swarm sends more inbound connections than the cap allows to queue.

## Suggested upstream fixes (any one suffices; first is smallest)

1. **Add an else-branch fallback to the `select!`** so all-disabled is reachable-safe, e.g.:
   ```rust
   , else => { /* both disabled: wait on a short timer and re-arm */ }
   ```
   or restructure the loop so the drain branch always matches on completion:
   ```rust
   Some(res) = futs.next(), if !futs.is_empty() => {
       if let Err(e) = res { debug!("handshake check failed: {e:#}"); }
       // res is Result<..., ...>; handle Err by simply not adding the peer.
   }
   ```
   Note this is exactly what **other** FuturesUnordered select! sites in the same crate already do (e.g. `session.rs`'s persistence-add loop uses `Some(res) = futs.next(), if !futs.is_empty()` and matches on `Result`); `task_listener` is the outlier that pattern-matches the inner `Ok` instead.

2. **Bound the queue by *pop*, not by *push*:** on failed handshake checks, remove the future from `futs` (or use a stream that surfaces every completion, success or failure), so the accept branch's guard can never dead-lock against a stuck queue.

3. **Never disable accept** — drop the `if futs.len() < cap` guard and instead shed load by closing the freshly accepted socket immediately when `futs.len() >= cap` (TCP backpressure via RST/queue-full), rather than by disabling the accept branch.

Any of (1)–(3) eliminates the all-disabled state; (2) additionally restores the cap's memory-bounding intent.

## Workarounds available to embedders (what keep-at did before the fix lands)

- **Raise `max_pending_incoming_handshake_checks` to `usize::MAX`** via `ListenerOptions`. This keeps the accept branch permanently enabled, so the `select!` can never go all-disabled. The handshake queue is still bounded in practice by inbound connection rate and by each future's own ~5s timeout (`live_wait_initializing`). This is what keep-at ships now (`listener_options` in `src/engine/session.rs`); it converts a crash into a (rare, bounded) memory growth case.
- Alternatively, keep the default cap and accept the (small but real) crash probability on large-library boots.

## Environment

- keep-at v0.8.17-beta (daemon binary), librqbit **9.0.1** from crates.io.
- Confirmed present in rqbit git master as of 2026-09-12 (fetched `crates/librqbit/src/session.rs`, `task_listener` unchanged).
- Rust 1.94.1, Linux, `panic = "abort"` profile.

## Contact

Filed by the keep-at maintainer (tweedge). Happy to provide the full daemon log excerpt, host details, or a reproducer description if useful — see https://github.com/tweedge/keep-at.
