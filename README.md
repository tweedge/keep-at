# keep-at

keep-at is a standalone Go daemon that seeds [Academic Torrents](https://academictorrents.com) automatically. Point it at some disk space, and it fills that space with whatever's most in need of seeding right now, favoring torrents with few seeds over torrents that are already healthy. The goal is to spread seeding load across the AT catalog instead of everyone piling onto the same popular torrents while obscure datasets rot with one seed (or fall to zero seeds and are lost).

It's built on [anacrolix/torrent](https://github.com/anacrolix/torrent) and runs on whatever you've got: a Raspberry Pi, a whole server, a VM, or a container.

## Usage

### Installing

The easiest way, on Linux or macOS:

```
curl -fsSL https://raw.githubusercontent.com/tweedge/keep-at/main/scripts/install.sh | sh
```

That fetches the latest [release](https://github.com/tweedge/keep-at/releases), picks the right binary for your OS/architecture, and installs it to `/usr/local/bin` (or `~/.local/bin` if you're not root and can't write there). Pass `VERSION=v1.2.3` before the pipe to install a specific version instead of the latest. Prebuilt binaries cover Linux (amd64/arm64/arm/386), macOS (amd64/arm64), and Windows (amd64); the install script itself only handles Linux and macOS, since it's meant to be piped straight into `sh`. On Windows, download the `.tar.gz` for your architecture from the [releases page](https://github.com/tweedge/keep-at/releases) and extract it manually.

Or run the prebuilt image in Docker, no Go toolchain needed:

```
docker run -v ./data:/data -v ./storage:/storage ghcr.io/tweedge/keep-at:latest --storage-limit 500G
```

Building from source (needs Go 1.26+) is only necessary if you're modifying keep-at itself:

```
go build -o keep-at ./cmd/keep-at
```

or to build the Docker image locally instead of pulling it:

```
docker build -t keep-at .
docker run -v ./data:/data -v ./storage:/storage keep-at --storage-limit 500G
```

### Quick Start

A config file is optional. Every setting has a flag, and there's a sensible default storage location for your OS already - the only thing keep-at won't guess is how much space you're willing to give it:

```
keep-at run --storage-limit 500G
```

That's the only variable you need to set to start. `keep-at run --help` lists every other flag (port, aggressiveness, scan interval, rate limit, and so on), all with reasonable defaults.

### Service Usage

keep-at is designed to be used as a long-running service on an always-on server, VM, or similar. Where `keep-at run` starts it in the foreground (good for seeing what's going on), `service install` is what you want for something that stays up. As a systemd service (Linux only for now; the binary itself is built for every platform above, but service install/uninstall is Linux-first):

```
sudo keep-at service install --storage-limit 500G
sudo keep-at service uninstall
```

`service install` takes the same flags as `run`, resolves them the same way, and writes the result to `/etc/keep-at/config.yaml` - installing the config alongside the service, rather than baking flags into the unit or requiring you to remember them. That's also what makes every other command below work with no arguments at all: once that file exists, `stop`, `status`, `network-status`, `hosted-torrents`, and even a bare `run`/`start` all check it automatically to find out where the running instance lives.

```
keep-at start
keep-at stop
keep-at status
keep-at network-status
keep-at hosted-torrents
```

`hosted-torrents` lists everything this host currently holds and seeds: title, actual space on disk, seeding/downloading status, last-scrape seeder and leecher counts, and a link to each torrent's Academic Torrents page. Like `status` and `network-status`, it reads keep-at's persisted files, so it works whether or not the daemon is running.

`start` and `run` take the exact same flags as `service install` - `start` just forks `run` into the background for you (or runs it in the foreground directly, inside a container). None of these commands need `--config` once keep-at is installed as a service; pass it explicitly only if you're managing a non-service instance, or one installed somewhere unusual.

To change settings later, edit `/etc/keep-at/config.yaml` directly and run `sudo systemctl restart keep-at`, or just run `service install` again with new flags.

And to update to the latest release:

```
keep-at self-update
```

### Advanced Configuration Settings

A config file is only worth reaching for once you want more than one storage location, or don't want to repeat flags every time (`service install` writes one for you automatically - see above). Point `--config` at a path that doesn't exist yet and keep-at will write a starter one there and tell you to edit it:

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
# Optional: your Academic Torrents API key - see below
api_key: "uid=12345;pass=abcdef..."
```

Every field here, plus its CLI flag equivalent and what it actually does, is documented in [docs/CONFIG.md](docs/CONFIG.md).

### Filling a dedicated drive: `limit: all`

A storage location's `limit` can be the literal `all` (or `--storage-limit all`) instead of a byte count. keep-at then resolves it at startup to **97.5% of the device's total formatted capacity** - it measures the filesystem with statfs and leaves the last 2.5% (plus whatever the filesystem itself reserves) for the journal, metadata, and the OS's emergency operations.

```
keep-at run --storage-limit all
```

```yaml
storage:
    locations:
        - path: /mnt/dedicated-drive/keep-at
          limit: all
```

> **DANGER: dedicated drives only. Never use `all` on an OS drive.** With `limit: all`, keep-at will attempt to fill the device to the resolved fraction - on a system disk that can choke the OS out of space for logs, swap, package managers, and the boot process itself. Use it only on a drive whose entire purpose is storing torrent data. A fixed byte limit is always safer if you're unsure.

Two things make `all` safe on a dedicated drive. First, keep-at's space accounting is **post-compression**: the limit is compared against actual on-disk bytes, so a torrent that compresses well (and much of Academic Torrents is textual or structured data) consumes less of the limit than its nominal size - compression gains become free space and are used to fit more torrents. Second, free-space checks are capped by what the device actually reports free, so filesystem overhead can't let accounting drift past real capacity.

### Stalled downloads free themselves

A torrent that falls to zero seeders and never completes can't ever finish - with no seeders, nobody can serve its missing pieces. keep-at tracks download progress per held torrent and, after `scan.stall_eviction_timeout` (default **two weeks**, configurable via `--stall-eviction-timeout` or `stall_eviction_timeout` in a config file; `0` disables), removes any held torrent that has stayed at zero seeders and gained no new pieces, freeing its slot and disk for a torrent that can actually complete. A torrent is only ever considered stalled once its clock has run a full quiet period - progress resets it - so slow-but-alive downloads are never evicted.

### Academic Torrents API Keys

*Optional.* keep-at works exactly the same with or without an API key - if you don't set one, you seed anonymously and nothing about what keep-at holds or how it behaves changes. The only difference is attribution.

Academic Torrents shows a "Hosted by" box on every torrent's details page listing the users who are hosting that data, and it associates a hoster with their account via a passkey embedded in the announce URL. If you'd like the torrents you seed to be credited to your account rather than shown anonymously, pass your API key (from https://academictorrents.com/my.php, formatted like `uid=12345;pass=abcdef...`):

```
keep-at run --api-key 'uid=12345;pass=abcdef...' --storage-limit 500G
```

keep-at announces to AT's tracker with that passkey so the attribution happens automatically. The key is only ever sent to Academic Torrents' own trackers (`academictorrents.com` and `ipv6.academictorrents.com`); third-party trackers never see it, and keep-at never logs it or writes it into cached torrent files. You can also set it in a config file as `api_key` (see below).

### VPN Compatibility

*Optional.* Running behind a VPN comes with real tradeoffs (mainly around port forwarding and speed) that are worth understanding before turning one on - see [docs/VPN.md](docs/VPN.md) for a general guide covering both Docker (via [gluetun](https://github.com/passteque/gluetun)) and service-level (WireGuard) setups.

## How It Works

keep-at ranks candidates by seed count, checks for other keep-at nodes already piling onto the same torrent before committing to one, and can combine several smaller held torrents to make room for one bigger candidate. The full rationale for all of that - plus the parts that are deliberately simplified or not implemented yet - is in [docs/DESIGN.md](docs/DESIGN.md).

## Testing

`go test ./...` covers everything except two tests that talk to real Academic Torrents infrastructure and are skipped by default:

```
KEEPAT_LIVE_TEST=1 go test ./internal/attorrent/...     # one real tracker scrape
KEEPAT_SMOKE_TEST=1 go test ./internal/engine/...        # full scan against a couple of real, small, already-seeded torrents
```

The smoke test downloads two real files from Academic Torrents (a few KB each) into a temp directory under a 1GB cap and confirms they land on disk compressed and correct.

## Releasing

Update `RELEASE_NOTES.md` at the repo root with what's actually in the release, commit it, then push a matching version tag. Always soft-wrap `RELEASE_NOTES.md` - each paragraph or bullet on one line, no matter how long, letting the renderer wrap it - never hard-wrap with manual line breaks partway through a paragraph. GitHub's release view renders single trailing newlines as literal breaks, so a hard-wrapped paragraph shows up as a jagged staircase instead of a normal paragraph.

```
git tag v1.2.3
git push origin v1.2.3
```

Two GitHub Actions workflows watch for tags matching `v*.*.*`: `.github/workflows/release.yml` cross-compiles every platform in `scripts/build-release.sh` and publishes them as a GitHub release using `RELEASE_NOTES.md` as the release notes, and `.github/workflows/docker.yml` builds a multi-arch (amd64/arm64) image and pushes it to `ghcr.io/tweedge/keep-at` tagged with the version, the `major.minor`, and `latest`. Neither needs any repo secrets - both run entirely on the `GITHUB_TOKEN` Actions provides automatically.
