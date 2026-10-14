# Vendoring rqbit

keep-at does not build rqbit from crates.io. It builds it from a dedicated
**vendored branch** on our rqbit fork, wired in as a git submodule at `vendor/`
and consumed through `[patch.crates-io]` in the workspace `Cargo.toml`.

## Where it is

- **Repository:** https://github.com/tweedge/rqbit (fork of https://github.com/ikatson/rqbit)
- **Branch:** `vendored-rqbit-9.0.1-keep-at-0`
- **Submodule path:** `vendor/` (`.gitmodules` records the branch; the gitlink
  pins the exact commit)
- **Patched crates** (see `Cargo.toml`): `librqbit`, `librqbit-bencode`,
  `librqbit-core`

## What is on the vendored branch

Base: upstream `main` at `57e95d16` ("fix(peer): read the handshake until it is
complete" — v9.0.1 + 25 commits; workspace version still `9.0.1`).

Applied on top (our open upstream PRs, merged and conflicts resolved):

| PR | Branch | Summary |
|----|--------|---------|
| [#667](https://github.com/ikatson/rqbit/pull/667) | `fix/peers-map-unbounded-growth` | bound the peers map; don't accept incoming peers for finished torrents with upload disabled (unbounded memory growth) |
| [#670](https://github.com/ikatson/rqbit/pull/670) | `feat/deferred-integrity-checks` | `check_after_load` + on-demand recheck API; also carries `fix(storage): re-open released files on demand` and `fix(session): preserve streams across recheck` |
| [#701](https://github.com/ikatson/rqbit/pull/701) | `fix/hybrid-pad-filenames` | don't reject duplicate `.pad/<length>` filenames (BEP-52 hybrid torrents couldn't be added) |

Conflict resolution so far: `crates/librqbit/src/tests/mod.rs` (all three PRs add
a test module) — both/all module declarations kept. Treat PR commits as the
source of record but adapt by hand when merging; do not rewrite them.

## Branch naming scheme

```
vendored-rqbit-<rqbit version>-keep-at-<counter>
```

- `<rqbit version>`: the upstream workspace version being vendored (e.g. `9.0.1`,
  later `9.0.3`).
- `<counter>`: increments by one for every new vendored branch ever created,
  regardless of rqbit version. The first is `keep-at-0`; the next is
  `keep-at-1`; if rqbit later releases `9.0.3` the branch after that is
  `vendored-rqbit-9.0.3-keep-at-2`, and so on.

## Creating the next vendored branch

1. `git fetch upstream` and branch from the new upstream `main`:
   `git checkout -b vendored-rqbit-<ver>-keep-at-<n> upstream/main`
2. Merge in the current set of pending upstream PRs (and any additional
   deltas keep-at needs that upstream lacks). Resolve conflicts; keep the
   vendor delta minimal and documented in this file.
3. Verify (see below).
4. Push to `tweedge/rqbit`.
5. In keep-at: `git -C vendor fetch && git -C vendor checkout
   vendored-rqbit-<ver>-keep-at-<n>`, run the tests, and commit the gitlink
   bump.
6. Update this file (branch name, base commit, applied PR/delta table).

Upstream-first stance: prefer landing fixes as upstream PRs and merging those
PR branches here; keep direct vendor-only edits to a minimum and list them
above when they exist. Patches for upstreaming are regenerated from the branch
delta as needed, e.g. `git format-patch upstream/main..vendored-rqbit-...`.

## Verifying the vendored branch

Matches upstream's CI (`.github/workflows/test.yml` in the vendored tree; note
its push trigger only covers `main`/`dev`, so run these locally or via PR):

```
cargo check
cargo fmt --all -- --check
cargo clippy --all-targets
cargo test
```

On Debian/Ubuntu the default feature set needs `pkg-config` + `libssl-dev`.
keep-at itself builds the crates with `default-features = false`
(`rust-tls`/`sha1-ring`), but the vendored workspace is verified with its
default features so it stays upstream-faithful.

Then verify the consumer: run `cargo test` in the keep-at workspace.

## Submodule mechanics

The submodule pins a **commit** (that is what gitlinks do); `branch` in
`.gitmodules` is informational, for `git submodule update --remote` workflows.
Upgrading is explicit: check out the desired ref inside `vendor/`, test, and
commit the gitlink change in keep-at.
