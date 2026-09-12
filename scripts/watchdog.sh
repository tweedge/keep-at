#!/bin/bash
# Reference watchdog for keep-at: restarts the daemon if it dies, and
# captures triage evidence first so any kill leaves a diagnosable packet.
#
# Install: copy to the host, fill in the env block, `chmod +x`, and add a
# cron line like:
#   */5 * * * * flock -n $KA_HOME/data/watchdog.lock /path/to/watchdog.sh
# flock prevents overlapping runs. The daemon writes its own pid file; the
# pid-file liveness gate must be checked before any pgrep fallback (a bare
# `pgrep -f keep-at` matches this script's own command line).
set -u

KA_HOME="${KA_HOME:-$HOME/keep-at}"
KABIN="$KA_HOME/keep-at"
DATADIR="$KA_HOME/data"
STORDIR="$KA_HOME/storage"
LOG="$DATADIR/watchdog.log"
PORT="${KA_PORT:-37550}"
STORAGE_LIMIT="${KA_STORAGE_LIMIT:-1G}"
MAX_RAM="${KA_MAX_RAM:-}"
SCAN_INTERVAL="${KA_SCAN_INTERVAL:-24h}"

ts() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }

if [ ! -x "$KABIN" ]; then
  echo "$(ts) watchdog: binary missing at $KABIN" >> "$LOG"
  exit 0
fi

# Liveness gate: pid file first, then an exact binary+verb pgrep as fallback.
if [ -f "$DATADIR/keep-at.pid" ] && kill -0 "$(cat "$DATADIR/keep-at.pid" 2>/dev/null)" 2>/dev/null; then
  exit 0
fi
if pgrep -f "$KABIN run --storage-location $STORDIR" >/dev/null 2>&1; then
  exit 0
fi

echo "$(ts) watchdog: death detected, capturing evidence" >> "$LOG"
# triage-last-exit reads the cgroup memory state (survives the process) and
# the last heartbeat; a 0.8.15+ daemon has this subcommand. On older builds
# this fails harmlessly and the restart still proceeds.
"$KABIN" triage-last-exit --data-dir "$DATADIR" >> "$LOG" 2>&1 \
  || echo "$(ts) watchdog: triage-last-exit failed (pre-0.8.15 build?)" >> "$LOG"

echo "$(ts) watchdog: starting" >> "$LOG"
mkdir -p "$DATADIR" "$STORDIR"
RAM_ARGS=""
[ -n "$MAX_RAM" ] && RAM_ARGS="--max-ram $MAX_RAM"
setsid nohup "$KABIN" run \
  --storage-location "$STORDIR" \
  --storage-limit "$STORAGE_LIMIT" \
  --data-dir "$DATADIR" \
  --port "$PORT" \
  $RAM_ARGS \
  --scan-interval "$SCAN_INTERVAL" \
  >> "$DATADIR/boot-scan.log" 2>&1 &
echo "$(ts) watchdog: started pid $!" >> "$LOG"
