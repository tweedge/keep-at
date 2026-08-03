# keep-at

keep-at is a standalone Go daemon that seeds [Academic Torrents](https://academictorrents.com) automatically - the name is "keep academic torrents at seeding." Point it at some disk space, and it fills that space with whatever's most in need of seeding right now, favoring torrents with few seeds over torrents that are already healthy. The goal is to spread seeding load across the AT catalog instead of everyone piling onto the same popular torrents while obscure datasets rot with one seed.

It's built on [anacrolix/torrent](https://github.com/anacrolix/torrent) and runs on whatever you've got: a Raspberry Pi, a home server, a Debian VM, or a container.

## Using keep-at

### Installing

Prebuilt binaries for common platforms (Linux amd64/arm64/arm/386, macOS amd64/arm64, Windows amd64) come from `scripts/build-release.sh`, which cross-compiles and packages each target the same way `keep-at self-update` expects to find them on GitHub releases.

To build from source, you need Go 1.26+:

```
go build -o keep-at ./cmd/keep-at
```

### Quick start

A config file is optional. Every setting has a flag, and there's a sensible default storage location for your OS already - the only thing keep-at won't guess is how much space you're willing to give it:

```
keep-at run --storage-limit 500G
```

That's enough to get going. `keep-at run --help` lists every other flag (port, aggressiveness, scan interval, rate limit, and so on), all with reasonable defaults.

Run it in Docker instead:

```
docker build -t keep-at .
docker run -v ./data:/data -v ./storage:/storage keep-at --storage-limit 500G
```

Inside a container, `start` automatically runs in the foreground instead of daemonizing - daemonizing would just exit the entrypoint and kill the container.

### Running it day to day

```
keep-at start --storage-limit 500G   # daemonize, write logs to data_dir/keep-at.log
keep-at stop
keep-at status
keep-at run --storage-limit 500G     # same thing, but stay in the foreground
```

`start` and `run` take the exact same flags - `start` just forks `run` into the background for you (or runs it in the foreground directly, inside a container). `stop` and `status` only need to know where to find the running instance, so they just take `--data-dir` (or `--config`, if you used one to start it).

Check what keep-at has seen of the wider keep-at network while scanning:

```
keep-at network-status
```

As a systemd service (Linux only for now; the binary itself is built for every platform above, but service install/uninstall is Linux-first):

```
sudo keep-at service install --storage-limit 500G
sudo keep-at service uninstall
```

`service install` takes the same flags as `run` and bakes the resolved settings directly into the systemd unit, so the service doesn't depend on anything you typed at install time still being around later.

And to update to the latest release:

```
keep-at self-update
```

### A config file, if you want one

A config file is only worth reaching for once you want more than one storage location, or don't want to repeat flags every time. Point `--config` at a path that doesn't exist yet and keep-at will write a starter one there and tell you to edit it:

```
keep-at run --config ~/.config/keep-at/config.yaml
```

which looks like:

```yaml
port: 37550
data_dir: /home/you/.local/share/keep-at
storage:
    locations:
        - path: /mnt/disk1/keep-at
          limit: 500G
        - path: /mnt/disk2/keep-at
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

Pass it with `--config path/to/config.yaml`. A couple of fields aren't self-explanatory:

* `storage.locations` - as many folders as you want, each with its own limit (`M`, `G`, `T`, or `P`, binary units). Multiple locations are a config-file-only feature; the CLI flags manage a single location. keep-at fills them proportionally to free space, not sequentially, so multiple disks fill roughly evenly instead of one disk taking everything until it's full.
* `scan.rate_limit_per_second` - caps requests specifically to Academic Torrents' own infrastructure (the catalog, `.torrent` downloads, and their tracker's scrape endpoint). Third-party trackers listed in a `.torrent` file aren't rate-limited by keep-at, since they're not Academic Torrents' infrastructure to protect.
* `keyword_blocklist` - case-insensitive substring match against a torrent's title and description (the only text fields the bulk catalog file actually provides; use `--keyword-blocklist a,b,c` from the CLI). Anything matching is skipped entirely, no network calls made.
* `preserve_deleted_torrents` - if Academic Torrents removes a torrent keep-at is seeding, keep-at removes it too by default, on the theory that if it was taken down there was probably a reason. Set this to keep seeding it anyway.

## How it works

keep-at ranks candidates by seed count, checks for other keep-at nodes already piling onto the same torrent before committing to one, and can combine several smaller held torrents to make room for one bigger candidate. The full rationale for all of that - plus the parts that are deliberately simplified or not implemented yet - is in [DESIGN.md](DESIGN.md).

## Testing

`go test ./...` covers everything except two tests that talk to real Academic Torrents infrastructure and are skipped by default:

```
KEEPAT_LIVE_TEST=1 go test ./internal/attorrent/...     # one real tracker scrape
KEEPAT_SMOKE_TEST=1 go test ./internal/engine/...        # full scan against a couple of real, small, already-seeded torrents
```

The smoke test downloads two real files from Academic Torrents (a few KB each) into a temp directory under a 1GB cap and confirms they land on disk compressed and correct.
