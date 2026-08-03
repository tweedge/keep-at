#!/usr/bin/env bash
# Cross-compiles mimis for every architecture PLAN.md asks for and packages
# each into mimisbaeti_<os>_<arch>.tar.gz, the naming convention
# internal/updater expects when checking GitHub releases for self-update.
set -euo pipefail

cd "$(dirname "$0")/.."

OUT_DIR="${OUT_DIR:-dist}"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

VERSION="${VERSION:-$(git describe --tags --always --dirty 2>/dev/null || echo dev)}"
COMMIT="${COMMIT:-$(git rev-parse --short HEAD 2>/dev/null || echo unknown)}"

LDFLAGS="-X github.com/tweedge/mimisbaeti/internal/buildinfo.Version=${VERSION} -X github.com/tweedge/mimisbaeti/internal/buildinfo.Commit=${COMMIT}"

# linux covers the Raspberry Pi / home server / Debian VM / Docker targets
# this project cares about most; darwin and windows are included too since
# PLAN.md asks for "all common architectures" without restricting to Linux.
#
# 32-bit ARM only ships one build at GOARM=6: Go's runtime.GOARCH reports
# "arm" regardless of GOARM version, so self-update (internal/updater)
# can't tell a v6 and v7 asset apart by architecture name alone. GOARM=6
# binaries still run fine on v7 hardware (just without v7-only
# optimizations), so it's the one to ship for broadest compatibility across
# every Raspberry Pi model.
TARGETS=(
  "linux amd64 "
  "linux arm64 "
  "linux arm 6"
  "linux 386 "
  "darwin amd64 "
  "darwin arm64 "
  "windows amd64 "
)

for target in "${TARGETS[@]}"; do
  read -r os arch goarm <<<"$target"
  name="mimisbaeti_${os}_${arch}"

  echo "building ${name}..."
  build_dir="$(mktemp -d)"
  binary_name="mimis"
  if [ "$os" = "windows" ]; then
    binary_name="mimis.exe"
  fi

  GOOS="$os" GOARCH="$arch" GOARM="$goarm" \
    go build -trimpath -ldflags "$LDFLAGS" -o "${build_dir}/${binary_name}" ./cmd/mimis

  tar -C "$build_dir" -czf "${OUT_DIR}/${name}.tar.gz" "$binary_name"
  rm -rf "$build_dir"
done

echo "done. artifacts in ${OUT_DIR}/"
ls -la "$OUT_DIR"
