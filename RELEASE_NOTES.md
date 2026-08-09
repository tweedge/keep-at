# keep-at release notes

## v0.5.3 - honest transfer stats, age-gate off switch, foreground status

### Transfer stats now separate useful data from total network traffic

"Downloaded since boot" previously counted every piece chunk received over the wire, including duplicate/wasted chunks from the swarm - which is why a naive reading could show far more "downloaded" than what ended up on disk. keep-at now reports both figures, in the periodic runtime-stats log and in `keep-at status`:

- **useful** - the piece data that actually mattered (bytes sent to peers that requested them; bytes received that keep-at needed),
- **total network** - everything over peer connections since boot, useful payload plus protocol overhead, handshakes, and duplicate/wasted chunks. The gap is the cost of swarming.

Both figures also come with **average rates since boot** (total-network bytes over uptime, in bps/kbps/mbps/gbps), so the log and status show e.g. `uploaded_useful=5.0G uploaded_total=5.2G upload_rate_avg=500 kbps`.

### Age gate can be fully disabled

`moderation_delay: 0` (or `--moderation-delay 0`) now disables the moderation-age gate entirely, including letting through torrents with no posted creation date. Previously a zero `createdAt` was always treated as ineligible even when the delay was off.

### `keep-at status` detects foreground runs

`keep-at status` previously reported "not running" for an instance started with `keep-at run` in the foreground, because only daemonized `keep-at start` writes a PID file. It now scans the process table and reports `keep-at is running in the foreground (pid N), not as a service` when it finds one using the same data dir.
