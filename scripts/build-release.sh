#!/usr/bin/env bash
# Cross-compiles keep-at (Rust) for Linux targets and packages each into
# keep-at_linux_<arch>.tar.gz, the naming self-update expects.
# Linux-only (the Rust migration dropped macOS/Windows).
#
# Needs: cargo + rustup targets installed once:
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
#     armv7-unknown-linux-musleabihf i686-unknown-linux-musl
# musl targets produce fully static binaries (no OpenSSL needed: rustls/ring).
#
# NOTE: aws-lc (via rustls default features) needs a musl-aware C compiler
# per target triple when cross-compiling. On Debian/Ubuntu runners:
#   sudo apt-get install -y musl-tools musl-dev gcc-aarch64-linux-gnu \
#     gcc-arm-linux-gnueabihf gcc-i686-linux-gnu
# and export the matching CC_<triple_underscored> (done below automatically).
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

# target triple -> asset arch -> musl C compiler package prefix
TARGETS=(
  "x86_64-unknown-linux-musl amd64 x86_64-linux-musl"
  "aarch64-unknown-linux-musl arm64 aarch64-linux-musl"
  "armv7-unknown-linux-musleabihf arm arm-linux-musleabihf"
  "i686-unknown-linux-musl 386 i686-linux-musl"
)

for target in "${TARGETS[@]}"; do
  read -r triple arch musl_cc_prefix <<<"$target"
  name="keep-at_linux_${arch}"

  echo "building ${name} (${triple})..."
  build_dir="$(mktemp -d)"

  # Point cc at the musl-aware gcc for this triple (aws-lc-sys honors CC).
  cc_var="CC_${triple//-/_}"
  export "${cc_var}=${musl_cc_prefix}-gcc"

  cargo build --release --target "$triple"
  cp "target/${triple}/release/keep-at" "${build_dir}/keep-at"

  tar -C "$build_dir" -czf "${OUT_DIR}/${name}.tar.gz" keep-at
  rm -rf "$build_dir"
done

echo "done. artifacts in ${OUT_DIR}/"
ls -la "$OUT_DIR"
