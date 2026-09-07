# Testing keep-at

Three tiers. Each has one job; none substitutes for another.

## Tier 1 — unit tests (`cargo test --lib`)

Pure functions with no I/O: selection math (`selector`), RAM model (`ram`), byte-size parsing, tracker-URL truncation, API-key classification. Fast (milliseconds), zero flakes, run everywhere including CI. These pin formulas (`chance_math`, `floor_p10`, `size_bias_knees`) so refactors can't silently change the math.

Rules: no filesystem, no network, no sleeps, no timing assertions. If a test needs any of those, it belongs in tier 2.

## Tier 2 — hermetic integration tests (`cargo test --test <name>`)

Real `Engine::scan_once` against local stub servers on 127.0.0.1 — a fake catalog server plus a fake Academic-Torrents stub serving synthetic `.torrent` bytes and canned scrape responses. No live network, ever. Shared fixture in `tests/common/`: synthetic torrent generator (hand-rolled bencode, title-derived filenames so hashes differ), canned per-hash scrapes, temp data/storage dirs, unique ports, timeout wrappers.

What "good" looks like here:

- **Assert through the public surface only**: `scan_once`, `held_torrents()`, `last_scan_stats()`, and the state files on disk (`state.json`, `network-stats.json`, `scrape-cache.json`). Never privates, never log strings. A refactor that keeps behavior must keep the tests green.
- **Deterministic gates**: selector rolls use `thread_rng`, so tests force the gate open (`aggressiveness = 0.999999` in the shared `test_config`) and assert pipeline outcomes, or use distinct seeder counts where chance is exactly 1.0. Gate *math* stays covered by tier-1 unit tests. (Lesson learned the hard way: the live smoke test flaked when fixture seeder counts drifted 3 → 5 between runs.)
- **Distinct seeder counts where bias can't matter**: the size-bias tie-break depends on the machine's real RAM, so integration tests never assert tie order from an Engine scan. Bias direction stays in unit tests.
- **Fresh catalog per scan phase**: the catalog cache TTL is the scan interval, so multi-phase tests set `interval = 1s` (already in shared `test_config`) or phase 2 silently re-reads phase 1's catalog.
- **Timeout wrappers, generous bounds**: every scan runs inside `with_timeout` (minutes, not seconds) so a wedged test fails fast instead of hanging CI. No timing assertion tighter than 10x margin. The shutdown test asserts "resolves within 60s", never an exact duration.
- **Unique ports per test file** (`test_port` offsets) so parallel tests never share a listener.
- **One cut point per test file**: gate, swaps, accounting, maintenance, lifecycle, trackers, CLI, transfer each own one seam. Grug composes coverage; no giant world-test.
- **Injectable slowness**: anything with a production sleep that a test must exercise (the 60s tracker-429 backoff) takes the duration from `Options` (default 60s, tests pass ~10ms). If a new sleep lands in a code path tests cover, it gets an `Options` field the same day — a slow test that people stop running is worse than no test.

Current files: `scan_gate.rs` (urgency order, floor persist + gating), `swaps.rs` (margin, disk+RAM coverage, eviction order, file removal), `accounting.rs` (nominal-plus-buffer regression), `maintenance.rs` (stall eviction, deleted removal, preserve flag), `lifecycle.rs` (resume without dupes, shutdown promptness), `trackers.rs` (429 backoff/recovery, UDP skip, AT classification), `cli.rs` (binary-level contract via `assert_cmd`), `transfer.rs` (two-session byte transfer over a stub tracker).

## Tier 3 — live smoke tests (`KEEPAT_SMOKE_TEST=1`, `KEEPAT_SMOKE_SUBSET=1`)

Two tests, kept green on pain of clubbing. They talk to real Academic Torrents infrastructure, need live network, and are skipped by default (CI runs them only if the env vars are set). They prove the plumbing connects to reality; they do not pin decisions (see the determinism rule above). The suite never grows without discussion — scale coverage belongs in tier 2 with bigger fixtures, not in more live tests.

## Expectations

- New behavior ships with a tier-2 test. Bug fixes reproduce with a regression test *first* (watch it fail, then fix).
- `cargo test --locked` runs everything hermetic (lib + all integration targets); CI runs exactly that. Live smokes stay opt-in via env vars.
- Before pushing: `cargo test --locked`, `cargo clippy --locked --all-targets` (zero warnings), `cargo fmt --check`. The same three the release workflow enforces.
- A failing test is a stop-the-line event: fix it or revert it the same day. A skipped/`#[ignore]`d test needs a tracking note with a date, not silence.
