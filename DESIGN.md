# Design

This is the authoritative explanation of *why* keep-at behaves the way it does: the selection math, the deliberate simplifications, and what's still missing. The README covers how to run it; this covers how it thinks.

## How it decides what to seed

Every scan, keep-at pulls Academic Torrents' catalog (`database.xml`), filters out anything keyword-blocked or too young (see "Age, precisely" below), and checks the remaining candidates' tracker scrape data. A torrent needs at least one live seed to be considered "available" - keep-at won't start a download that can never finish.

Among available candidates, fewer seeds means higher priority. If keep-at has free space, it fills it with the highest-priority candidate it can. If space is full, it'll displace currently-held torrents for a new candidate, but only if the candidate has at least `min_seed_margin` (default 3) fewer seeds than everything it would replace.

## The anti-cascade check

Before committing to a download, keep-at checks how many other keep-at nodes are already in that torrent's swarm (it identifies itself in the BitTorrent extended handshake, and looks for peers reporting the same string). This exists to stop every keep-at node globally from swapping to the same under-seeded torrent at once and just moving the problem somewhere else. The chance keep-at proceeds is:

```
n = aggressiveness ^ (keep-at peers already in the swarm)
```

With zero other keep-at nodes present, n is 1: go ahead confidently. As more keep-at nodes join, n shrinks toward zero (aggressiveness defaults to 0.6, and is always between 0 and 1), so later nodes progressively back off. keep-at rolls a random float and proceeds only if the roll is below n.

This same anti-cascade check runs whether keep-at is filling free space or displacing something, so it also naturally discourages every keep-at node from racing to grab the same freshly-added torrent.

One correction worth documenting: an earlier draft of this design described rolling *above* n to proceed. That's backwards from what n is defined to mean ("the chance keep-at swaps") and produces the opposite of the intended effect - a shrinking n would make later nodes pile on *more* eagerly, not less. keep-at implements the definition, not the inverted comparison: `selector.EvaluateSwap` swaps when `roll < n`.

## Multi-torrent swaps

A candidate can be bigger than any single held torrent. When that happens, keep-at looks within one storage location for a *set* of held torrents to displace together, not just one:

1. Filter to held torrents that individually clear the seed margin against the candidate (`heldSeeders - min_seed_margin >= candidateSeeders`). A torrent that wouldn't qualify on its own never gets swept in just because it's bundled with others that do.
2. Sort the qualifying torrents by seeders descending - evict the least valuable ones (most-seeded, least in need of keep-at specifically) first.
3. Accumulate them in that order until their combined size covers the candidate. If even every qualifying torrent in that location combined isn't enough room, that location can't take the swap.

This is a greedy selection, not a minimal one - it doesn't search for the smallest possible subset that would fit, just the first prefix (by seeders descending) that does. That keeps the logic simple and predictable at the cost of occasionally evicting one more torrent than a smarter bin-packing solution would need.

## Availability, precisely

A torrent could theoretically be "available" with zero full seeds, if enough partial peers between them happen to cover 100% of the data. keep-at doesn't attempt to detect that case: verifying it means inspecting every peer's piece map across the whole swarm, which is expensive to do for every catalog candidate on every scan. keep-at uses the cheaper, conservative signal instead - at least one live seed - and accepts that this misses the rare fragmented-but-complete swarm.

## Age, precisely

Academic Torrents' `database.xml` doesn't include an upload date, and neither does their API - the closest thing, a paper's publication date, isn't the same thing and would make the moderation delay meaningless. Instead, keep-at reads the `creation date` field baked into each `.torrent` file at fetch time, which is stable and set when the torrent was created (verified against torrents from 2013 through 2026 while building this). A torrent needs to be at least `moderation_delay` old (default 7 days) before keep-at will touch it, giving Academic Torrents' moderators time to catch anything that shouldn't be there. If keep-at can't determine a torrent's age at all, it treats that as not yet eligible rather than assuming it's fine.

## Network-wide stats

While scanning, keep-at briefly joins the swarm of every candidate anyone at all is seeding or leeching - the same probe that counts keep-at peers for the anti-cascade check - and records, per keep-at peer found: its (best-effort) node identity, and whether it has the whole torrent (seeding) or not (leeching). `keep-at network-status` reports the totals.

This is necessarily an estimate, not a census:

* **Node count** is distinct IP addresses seen claiming to be keep-at, only across torrents scanned this run. A node sharing a NAT with another keep-at instance undercounts; comparing node counts across separate scans isn't a reliable trend line, since each scan only sees what it happened to probe.
* **Seeding/leeching byte totals** sum a torrent's full size once per keep-at node observed holding it complete or incomplete, deliberately not deduplicated across nodes - the point is total keep-at-attributable capacity in use, not unique data volume.
* A hostile peer could claim to be keep-at when it isn't; nothing about the extended handshake is authenticated.

Progress reporting (`processed/total candidates`) is based on how many catalog entries keep-at intends to walk through this scan (everything not already held and not keyword-blocked), computed before any network calls, so the denominator is stable even though which of those turn out to be age-eligible or scrapeable isn't known until each one is actually processed.

## Storage

keep-at stores each verified piece as its own gzip-compressed file, keyed by piece index under a directory named after the torrent's infohash. There's no attempt to reconstruct the original file layout on disk - stored data doesn't need to be locally readable, and giving up on that constraint is what makes per-piece compression simple. Deleting a torrent just removes its directory.

Cross-torrent deduplication (storing identical pieces once even if they appear in multiple torrents) was considered and deliberately left out. Exact piece-level duplicates across unrelated academic datasets are rare enough that the added complexity (a content-addressable store with reference counting and garbage collection) wasn't worth it for the space it'd actually save. Compression alone still helps a lot with the kind of data Academic Torrents hosts - text, tabular data, and other formats that compress well.

## What's not implemented yet

* **macOS and Windows service management.** The binary runs fine on both; `keep-at service install` doesn't (systemd/Linux only).
* **Peer-map availability.** See "Availability, precisely" above.
* **Authenticated node identity.** The anti-cascade check and network-status both trust the BitTorrent extended handshake's claimed client name at face value.
