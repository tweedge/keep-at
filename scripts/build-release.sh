#!/usr/bin/env bash
# Cross-compiles keep-at (Rust) for Linux targets and packages each into
# keep-at_linux_<arch>.tar.gz, the naming self-update expects.
# Linux-only (the Rust migration dropped macOS/Windows).
#
# Needs: cargo + rustup targets installed once:
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
#     armv7-unknown-linux-musleabihf i686-unknown-linux-musl
# musl targets produce fully static binaries (no OpenSSL needed: rustls/ring).
set -euo pipefail

cd "$(dirname "$0")/.."

OUT_DIR="${OUT_DIR:-dist}"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

VERSION="${VERSION:-$(git describe --tags --always --dirty 2>/dev/null || echo dev)}"

# Stamp the release version into the binary: buildinfo prefers
# KEEPAT_VERSION_OVERRIDE (compile-time env) over Cargo.toml.
if [[ "$VERSION" =~ ^v[0-9] ]]; then
  export KEEPAT_VERSION_OVERRIDE="${VERSION#v}"
else
  export KEEPAT_VERSION_OVERRIDE="$VERSION"
fi

# target triple -> asset arch
TARGETS=(
  "x86_64-unknown-linux-musl amd64"
  "aarch64-unknown-linux-musl arm64"
  "armv7-unknown-linux-musleabihf arm"
  "i686-unknown-linux-musl 386"
)

for target in "${TARGETS[@]}"; do
  read -r triple arch <<<"$target"
  name="keep-at_linux_${arch}"

  echo "building ${name} (${triple})..."
  build_dir="$(mktemp -d)"

  cargo build --release --target "$triple"
  cp "target/${triple}/release/keep-at" "${build_dir}/keep-at"

  tar -C "$build_dir" -czf "${OUT_DIR}/${name}.tar.gz" keep-at
  rm -rf "$build_dir"
done

echo "done. artifacts in ${OUT_DIR}/"
ls -la "$OUT_DIR"
