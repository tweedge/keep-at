# mimisbaeti

mimis is a standalone Go daemon that seeds [Academic Torrents](https://academictorrents.com) automatically. Point it at some disk space, and it fills that space with whatever's most in need of seeding right now, favoring torrents with few seeds over torrents that are already healthy. The goal is to spread seeding load across the AT catalog instead of everyone piling onto the same popular torrents while obscure datasets rot with one seed.

It's built on [anacrolix/torrent](https://github.com/anacrolix/torrent) and runs on whatever you've got: a Raspberry Pi, a home server, a Debian VM, or a container.

## How it decides what to seed

Every scan, mimis pulls Academic Torrents' catalog (`database.xml`), filters out anything keyword-blocked or too young (see below), and checks the remaining candidates' tracker scrape data. A torrent needs at least one live seed to be considered "available" - mimis won't start a download that can never finish.

Among available candidates, fewer seeds means higher priority. If mimis has free space, it fills it with the highest-priority candidate it can. If space is full, it'll displace a currently-held torrent for a new candidate, but only if the candidate has at least `min_seed_margin` (default 3) fewer seeds than the torrent it would replace.

Before committing to a download, mimis checks how many other mimis nodes are already in that torrent's swarm (it identifies itself in the BitTorrent extended handshake, and looks for peers reporting the same string). This exists to stop every mimis node globally from swapping to the same under-seeded torrent at once and just moving the problem somewhere else. The chance mimis proceeds is:

```
n = aggressiveness ^ (mimis peers already in the swarm)
```

With zero other mimis nodes present, n is 1: go ahead confidently. As more mimis nodes join, n shrinks toward zero (aggressiveness defaults to 0.6, and is always between 0 and 1), so later nodes progressively back off. mimis rolls a random float and proceeds only if the roll is below n.

One correction worth calling out: PLAN.md's original wording described rolling *above* n to proceed. That's backwards from what n is defined to mean ("the chance mimis swaps") and produces the opposite of the intended effect - a shrinking n would make later nodes pile on *more* eagerly, not less. mimis implements the definition, not the inverted comparison.

This same anti-cascade check runs whether mimis is filling free space or displacing something, so it also naturally discourages every mimis node from racing to grab the same freshly-added torrent.

### Availability, precisely

The plan calls for a torrent to be available if there's "1 live seed or peers with sufficient data" - meaning a swarm with zero full seeds but enough partial peers to reconstruct 100% of the data still counts. mimis doesn't attempt that second case: verifying it means inspecting every peer's piece map across the whole swarm, which is expensive to do for every catalog candidate on every scan. mimis uses the cheaper, conservative signal instead - at least one live seed - and accepts that this misses the rare case of a fragmented-but-complete swarm.

### Age, precisely

Academic Torrents' `database.xml` doesn't include an upload date, and neither does their API - the closest thing, a paper's publication date, isn't the same thing and would make the moderation delay meaningless. Instead, mimis reads the `creation date` field baked into each `.torrent` file at fetch time, which is stable and set when the torrent was created (verified against torrents from 2013 through 2026 while building this). A torrent needs to be at least `moderation_delay` old (default 7 days) before mimis will touch it, giving Academic Torrents' moderators time to catch anything that shouldn't be there. If mimis can't determine a torrent's age at all, it treats that as not yet eligible rather than assuming it's fine.

## Installing

Prebuilt binaries for common platforms (Linux amd64/arm64/arm/386, macOS amd64/arm64, Windows amd64) come from `scripts/build-release.sh`, which cross-compiles and packages each target the same way `mimis self-update` expects to find them on GitHub releases.

To build from source, you need Go 1.26+:

```
go build -o mimis ./cmd/mimis
```

Or run it in Docker:

```
docker build -t mimisbaeti .
docker run -v ./config.yaml:/etc/mimisbaeti/config.yaml -v ./data:/data -v ./storage:/storage mimisbaeti
```

Inside a container, `mimis start` automatically runs in the foreground instead of daemonizing - daemonizing would just exit the entrypoint and kill the container.

## Configuring

mimis refuses to run without a config file that sets at least one storage location and a limit. There's no default amount of space it's willing to use; that's a decision only you should make. Run it once without a config and it'll write a starter one and tell you where:

```yaml
port: 37550
data_dir: /home/you/.local/share/mimisbaeti
storage:
    locations:
        - path: /mnt/disk1/mimis
          limit: 500G
        - path: /mnt/disk2/mimis
          limit: 2T
scan:
    interval: 168h0m0s
    rate_limit_per_second: 0.5
    min_seed_margin: 3
    moderation_delay: 168h0m0s
aggressiveness: 0.6
keyword_blocklist: []
preserve_deleted_torrents: false
```

Notes on the fields that aren't self-explanatory:

* `storage.locations` - as many folders as you want, each with its own limit (`M`, `G`, `T`, or `P`, binary units). mimis fills them proportionally to free space, not sequentially, so multiple disks fill roughly evenly instead of one disk taking everything until it's full.
* `scan.rate_limit_per_second` - caps requests specifically to Academic Torrents' own infrastructure (the catalog, `.torrent` downloads, and their tracker's scrape endpoint). Third-party trackers listed in a `.torrent` file aren't rate-limited by mimis, since they're not Academic Torrents' infrastructure to protect.
* `keyword_blocklist` - case-insensitive substring match against a torrent's title and description (the only text fields the bulk catalog file actually provides). Anything matching is skipped entirely, no network calls made.
* `preserve_deleted_torrents` - if Academic Torrents removes a torrent mimis is seeding, mimis removes it too by default, on the theory that if it was taken down there was probably a reason. Set this to keep seeding it anyway.

## Running

```
mimis start              # daemonize, write logs to data_dir/mimis.log
mimis stop
mimis status
mimis run                # same thing, but stay in the foreground
```

As a systemd service (Linux only for now; the binary itself is built for every platform above, but service install/uninstall is Linux-first per PLAN.md):

```
sudo mimis service install
sudo mimis service uninstall
```

And to update to the latest release:

```
mimis self-update
```

## Storage

mimis stores each verified piece as its own gzip-compressed file, keyed by piece index under a directory named after the torrent's infohash. There's no attempt to reconstruct the original file layout on disk - the plan is explicit that stored data doesn't need to be locally readable, and giving up on that constraint is what makes per-piece compression simple. Deleting a torrent just removes its directory.

Cross-torrent deduplication (storing identical pieces once even if they appear in multiple torrents) was considered and deliberately left out. Exact piece-level duplicates across unrelated academic datasets are rare enough that the added complexity (a content-addressable store with reference counting and garbage collection) wasn't worth it for the space it'd actually save. Compression alone still helps a lot with the kind of data Academic Torrents hosts - text, tabular data, and other formats that compress well.

## What's not implemented yet

* **Multi-torrent swaps.** If a new candidate is bigger than any single held torrent, mimis won't combine several smaller held torrents to make room. It only does one-for-one swaps.
* **macOS and Windows service management.** The binary runs fine on both; `mimis service install` doesn't.
* **Peer-map availability.** See "Availability, precisely" above.

## Testing

`go test ./...` covers everything except two tests that talk to real Academic Torrents infrastructure and are skipped by default:

```
MIMIS_LIVE_TEST=1 go test ./internal/attorrent/...     # one real tracker scrape
MIMIS_SMOKE_TEST=1 go test ./internal/engine/...        # full scan against a couple of real, small, already-seeded torrents
```

The smoke test downloads two real files from Academic Torrents (a few KB each) into a temp directory under a 1GB cap and confirms they land on disk compressed and correct.
