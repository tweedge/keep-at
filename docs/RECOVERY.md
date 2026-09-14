# Recovery: the catalog collapse guard

## What the guard is for

Every scan, keep-at deletes any held torrent that the fresh Academic Torrents
catalog no longer lists - including the downloaded files, the state entry, and
the cached `.torrent` (unless `--preserve-deleted-torrents` is set). That
assumes the catalog response is trustworthy. It usually is, but it parses from
a remote feed: if Academic Torrents ever changes its schema (say, renames the
`infohash` field), every row is silently skipped during parsing and the daemon
sees a perfectly-formed catalog with **zero items** - and would wipe the whole
library in one scan.

Since 0.8.24, the removal pass is refused when a fresh catalog lists fewer
than `catalog_collapse_percent` percent of the currently held set:

- default: **30** (a fresh catalog must list at least 30% of held torrents);
- `0` disables the guard (old behavior);
- `100` is the strictest: the catalog must list *every* held torrent before
  any removal is allowed.

When the guard trips, the scan logs

```
catalog collapse guard: fresh catalog lists N items but M torrents are held ...
```

and keeps everything seeded. Nothing is deleted on a guarded scan; the guard
re-evaluates every scan, so a *real*, gradual catalog shrink below the
threshold also pauses removals until you adjust the setting.

## Tuning the knob

```
keep-at run --catalog-collapse-percent 50
```

or in the config file:

```yaml
catalog_collapse_percent: 50
```

- **Raise it** (e.g. 50-100) if you see the warning repeatedly and have
  verified on academictorrents.com that the removals are real.
- **Set 0** to disable the guard entirely (not recommended; prefer
  `--preserve-deleted-torrents` for a torrent-by-torrent equivalent).
- `0`-`100` enforced; anything else fails config validation.

## Manual recovery after a wipe

If an older build (pre-0.8.24) already wiped the library:

1. **The ledger survives.** `history.jsonl` in the data dir records every
   addition with its info-hash, title, and size:
   ```
   jq -r 'select(.event == "add") | "\(.hash)  \(.title)"' history.jsonl | sort -u
   ```
2. **Re-acquisition is automatic.** The daemon re-discovers the catalog on
   its next scans and re-adds every torrent it still lists there (moderation
   age and seed-scarcity gates apply again, so re-population is progressive).
   Data re-downloads from the swarm; nothing needs to be done by hand.
3. **Torrents no longer on Academic Torrents** cannot be re-added by the
   daemon (it only seeds catalog content). The hashes from step 1 tell you
   what is missing; those torrents are gone from the catalog and would need
   `--preserve-deleted-torrents` to have survived.
4. **Make the next recovery trivial**: keep an off-host copy of `state.json`
   and `history.jsonl`, e.g. a daily cron:
   ```
   cp ~/.local/share/keep-at/state.json ~/.local/share/keep-at/history.jsonl \
     /backups/keep-at/$(date +%F)/
   ```
   `state.json` alone is enough to restore holdings after a restart;
   `history.jsonl` is the durable ledger.
