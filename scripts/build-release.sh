#!/usr/bin/env bash
# Cross-compiles keep-at (Rust) for Linux targets and packages each into
# keep-at_linux_<arch>.tar.gz, the naming self-update expects.
# Linux-only.
#
# Binaries link glibc dynamically (gnu targets) - fine for any reasonably
# recent distro, the systemd unit, and the install script. Fully static musl
# builds were tried and dropped: aws-lc (rustls's crypto provider) does not
# link against musl's libc (undefined __memcpy_chk/__vsnprintf_chk at final
# link), and working around it would mean vendoring a musl gcc per target.
# Needs: cargo + rustup targets installed once:
#   rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
#     armv7-unknown-linux-gnueabihf i686-unknown-linux-gnu
# plus the matching C cross-compilers for the non-native targets:
#   sudo apt-get install -y gcc-aarch64-linux-gnu \
#     gcc-arm-linux-gnueabihf gcc-i686-linux-gnu
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

# target triple -> asset arch -> cross C compiler (for aws-lc-sys C build)
TARGETS=(
  "x86_64-unknown-linux-gnu amd64"
  "aarch64-unknown-linux-gnu arm64 aarch64-linux-gnu-gcc"
  "armv7-unknown-linux-gnueabihf arm arm-linux-gnueabihf-gcc"
  "i686-unknown-linux-gnu 386 i686-linux-gnu-gcc"
)

for target in "${TARGETS[@]}"; do
  read -r triple arch cross_cc <<<"$target"
  name="keep-at_linux_${arch}"

  echo "building ${name} (${triple})..."
  build_dir="$(mktemp -d)"

  # Point cc at the cross gcc for this triple (aws-lc-sys honors CC), and
  # use it as the Rust linker too: aws-lc-sys emits an AArch64-only
  # --fix-cortex-a53-843419 link arg on arm64, which the default cc-linker
  # (x86_64 rust-lld via collect2) rejects. Linking with the target gcc
  # keeps C objects and the final link under one consistent toolchain.
  if [ -n "${cross_cc:-}" ]; then
    command -v "$cross_cc" >/dev/null || {
      echo "missing cross compiler $cross_cc for $triple" >&2
      echo "install it, e.g.: sudo apt-get install -y gcc-aarch64-linux-gnu gcc-arm-linux-gnueabihf gcc-i686-linux-gnu" >&2
      exit 1
    }
    cc_var="CC_${triple//-/_}"
    export "${cc_var}=${cross_cc}"
    link_var="CARGO_TARGET_$(echo "$triple" | tr 'a-z-' 'A-Z_')_LINKER"
    export "${link_var}=${cross_cc}"
  fi

  cargo build --release --target "$triple"
  cp "target/${triple}/release/keep-at" "${build_dir}/keep-at"

  tar -C "$build_dir" -czf "${OUT_DIR}/${name}.tar.gz" keep-at
  rm -rf "$build_dir"
done

echo "done. artifacts in ${OUT_DIR}/"
ls -la "$OUT_DIR"
