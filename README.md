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

Inside a container, `start` automatically runs in the foreground instead of daemonizing - daemonizing would just exit the entrypoint and kill the container.

### Service Usage

keep-at is designed to be used as a long-running service on an always-on server, VM, or similar. Where `keep-at run` starts it in the foreground (good for seeing what's going on), `service install` is what you want for something that stays up. As a systemd service (Linux only for now; the binary itself is built for every platform above, but service install/uninstall is Linux-first):

```
sudo keep-at service install --storage-limit 500G
sudo keep-at service uninstall
```

`service install` takes the same flags as `run`, resolves them the same way, and writes the result to `/etc/keep-at/config.yaml` - installing the config alongside the service, rather than baking flags into the unit or requiring you to remember them. That's also what makes every other command below work with no arguments at all: once that file exists, `stop`, `status`, `network-status`, and even a bare `run`/`start` all check it automatically to find out where the running instance lives.

```
keep-at start
keep-at stop
keep-at status
keep-at network-status
```

`start` and `run` take the exact same flags as `service install` - `start` just forks `run` into the background for you (or runs it in the foreground directly, inside a container). None of these commands need `--config` or `--data-dir` once keep-at is installed as a service; pass them explicitly only if you're managing a non-service instance, or one installed somewhere unusual.

To change settings later, edit `/etc/keep-at/config.yaml` directly and run `sudo systemctl restart keep-at`, or just run `service install` again with new flags.

And to update to the latest release:

```
keep-at self-update
```

### A config file, if you want one

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
```

Every field here, plus its CLI flag equivalent and what it actually does, is documented in [docs/CONFIG.md](docs/CONFIG.md).

### Running behind a VPN

Optional, and it comes with real tradeoffs (mainly around port forwarding and speed) that are worth understanding before turning one on - see [docs/VPN.md](docs/VPN.md) for a general guide covering both Docker (via [gluetun](https://github.com/passteque/gluetun)) and service-level (WireGuard) setups.

## How it works

keep-at ranks candidates by seed count, checks for other keep-at nodes already piling onto the same torrent before committing to one, and can combine several smaller held torrents to make room for one bigger candidate. The full rationale for all of that - plus the parts that are deliberately simplified or not implemented yet - is in [docs/DESIGN.md](docs/DESIGN.md).

## Testing

`go test ./...` covers everything except two tests that talk to real Academic Torrents infrastructure and are skipped by default:

```
KEEPAT_LIVE_TEST=1 go test ./internal/attorrent/...     # one real tracker scrape
KEEPAT_SMOKE_TEST=1 go test ./internal/engine/...        # full scan against a couple of real, small, already-seeded torrents
```

The smoke test downloads two real files from Academic Torrents (a few KB each) into a temp directory under a 1GB cap and confirms they land on disk compressed and correct.

## Releasing

Pushing a version tag is all it takes:

```
git tag v1.2.3
git push origin v1.2.3
```

Two GitHub Actions workflows watch for tags matching `v*.*.*`: `.github/workflows/release.yml` cross-compiles every platform in `scripts/build-release.sh` and publishes them as a GitHub release, and `.github/workflows/docker.yml` builds a multi-arch (amd64/arm64) image and pushes it to `ghcr.io/tweedge/keep-at` tagged with the version, the `major.minor`, and `latest`. Neither needs any repo secrets - both run entirely on the `GITHUB_TOKEN` Actions provides automatically.
