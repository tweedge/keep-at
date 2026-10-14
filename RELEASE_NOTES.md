# keep-at release notes

## v0.8.34-beta - vendored rqbit mainline: peers-map bound and 25 upstream fixes

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

keep-at no longer builds rqbit from crates.io. It now builds `librqbit`,
`librqbit-bencode` and `librqbit-core` from a vendored branch of our rqbit
fork (`vendor/` submodule; see `docs/VENDORING.md`), based on upstream
mainline (v9.0.1 + 25 fixes) with our three open upstream PRs merged:

- **#667 — bound the peers map.** `PeerStates` grew without bound: entries
  were created for every incoming connection and every address learned from
  trackers/DHT/PEX and never removed. On long-lived daemons this showed up as
  creeping RSS. Incoming peers are now bounded and finished torrents with
  upload disabled stop accepting them.
- **#670 — deferred integrity checks (`check_after_load`) and an on-demand
  recheck API.** Opt-in; keep-at's boot behavior is unchanged (every boot
  still re-verifies the library). Also carries `fix(storage): re-open released
  files on demand` and `fix(session): preserve streams across recheck`.
- **#701 — BEP-52 hybrid `.pad/<length>` filenames.** Hybrid v1+v2 torrents
  whose v1 compatibility list repeats a padding path are now accepted
  (padding files are never materialized, so equal pad names are legal per
  BEP 47/52). This replaces the minimal in-tree fix from v0.8.33-beta with
  the full upstream-ready implementation and its end-to-end tests.

Plus 25 upstream fixes since v9.0.1, notably: no panic in the incoming
connection listener, `pwritev` past 2 GiB, chunk-tracker queue correctness,
bencode recursion-depth limits, incoming-handshake capability detection, and
several storage-wrapper regressions.

### What did not change

- Boot restore still re-verifies the library on every start; the deferred-check
  API is available but not adopted.
- Tracker/announce policy, quarantine, scanning, and selection behavior are
  untouched; this release is dependency and correctness work.

### Verification

The vendored branch passes upstream's check sequence (`cargo check`, `fmt`,
`clippy`, `test`) and keep-at's full test suite passes against it (123 lib
tests plus every integration suite).



## v0.8.32-beta - quarantine re-probe runs on the watchdog, not at scan time

This is a beta release for field validation; the next stable cut will be identical apart from the version tag.

One behavior change, aimed at the gap this release was prompted by: a quarantine cooldown that lapsed into a long wait.

### A lapsed quarantine is re-tested in minutes, not whenever the next scan happens

When a broken swarm is quarantined, keep-at waits a cooldown (`scan.quarantine_cooldown`, default 3 days) and then re-tests the torrent — if the uploader fixed their seeders' data, the download completes against the registered piece hashes and the quarantine lifts automatically.

That re-test was documented to happen when the cooldown expired. In practice it happened when the **next scan** ran, because the re-add only existed inside the scan's candidate evaluation. On a host scanning every 7 days, a 3-day cooldown really meant "re-test after up to 10 days". Mercury sat in exactly that gap in early October: the cooldown lapsed on the 7th and the next scan was not due until the 10th.

The re-test now runs on the quarantine watchdog (default every 30 minutes), which already owned the rest of the quarantine lifecycle — detecting broken swarms, counting attempts, lifting recovered entries, and garbage-collecting orphaned ones. The whole cycle is timer-driven now, so the worst case for re-testing is one watchdog tick.

### What did not change

- **A re-test is still free-space only.** A probe is speculative — expected to fail and re-quarantine — and a swap deletes the displaced torrent's data. A probe never displaces a healthy held torrent; on a full node it waits for room instead of trading a good holding for a likely failure. This rule is unchanged and now covered by a regression test on the watchdog path as well as the scan path.
- **A re-test still does not count as a retry.** `attempts` counts quarantines (trips), so `scan.quarantine_cooldown` remains the clock that paces `scan.quarantine_max_retries` escalation. Re-testing sooner cannot lock out a recoverable torrent any faster than before.
- **The seed-scarcity bypass is unchanged.** A broken-but-well-seeded swarm is still admitted for re-testing even though the politeness gate would otherwise price it at roughly zero chance.

### Guarding the new cadence

Each re-test attempt re-arms the cooldown, floored at `scan.quarantine_check_interval`, so there is at most one re-test per cooldown period no matter how the knobs are set. That matters because `scan.quarantine_cooldown: 0` is a legal setting meaning "no cooldown": without the floor it would have re-downloaded a known-poisoned swarm on every watchdog tick, which is precisely the waste the quarantine exists to stop.

The watchdog only runs when `scan.quarantine_check_interval` is non-zero, so setting it to `0` still disables quarantine automation entirely, as before.

### Regression coverage

- A re-test that happens with no scan able to explain it, asserting also that `attempts` is untouched, the cooldown is re-armed, the `quarantine_cooldown: 0` floor holds, and a second pass does not double-add. The probe in this test is priced at chance exactly zero, so it also pins the seed-scarcity bypass.
- A probe deferred rather than displacing a healthy held torrent on the watchdog path, mirroring the existing test for the scan path.
