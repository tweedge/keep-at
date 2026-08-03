#!/bin/sh
# Installs the latest (or a specific) keep-at release for Linux or macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/tweedge/keep-at/main/scripts/install.sh | sh
#
# Override the version or install location with environment variables:
#
#   VERSION=v1.2.3 INSTALL_DIR=~/bin sh install.sh
set -e

REPO="tweedge/keep-at"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-}"

say() { echo "keep-at install: $*"; }
die() {
  echo "keep-at install: $*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "this script needs '$1', which isn't on your PATH"
}
need curl
need tar

os="$(uname -s)"
case "$os" in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  *) die "unsupported OS '$os' - this script only supports Linux and macOS. Download a release manually from https://github.com/${REPO}/releases" ;;
esac

arch="$(uname -m)"
case "$arch" in
  x86_64 | amd64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  armv6l | armv7l | arm) arch=arm ;;
  i386 | i686) arch=386 ;;
  *) die "unsupported architecture '$arch'. Download a release manually from https://github.com/${REPO}/releases" ;;
esac

if [ "$VERSION" = "latest" ]; then
  say "checking for the latest release..."
  version="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep -o '"tag_name": *"[^"]*"' | head -1 | cut -d '"' -f4)"
  [ -n "$version" ] || die "could not determine the latest release from the GitHub API"
else
  version="$VERSION"
fi

asset="keep-at_${os}_${arch}.tar.gz"
url="https://github.com/${REPO}/releases/download/${version}/${asset}"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT INT TERM

say "downloading ${url}"
curl -fsSL "$url" -o "${tmpdir}/${asset}" || die "download failed - is '${version}' a real release, and does it have a ${os}/${arch} build?"

tar -C "$tmpdir" -xzf "${tmpdir}/${asset}"
[ -f "${tmpdir}/keep-at" ] || die "downloaded archive did not contain a keep-at binary"

if [ -z "$INSTALL_DIR" ]; then
  if [ "$(id -u)" = "0" ] || [ -w /usr/local/bin ]; then
    INSTALL_DIR=/usr/local/bin
  else
    INSTALL_DIR="${HOME}/.local/bin"
  fi
fi

mkdir -p "$INSTALL_DIR"
mv "${tmpdir}/keep-at" "${INSTALL_DIR}/keep-at"
chmod +x "${INSTALL_DIR}/keep-at"

say "installed ${version} to ${INSTALL_DIR}/keep-at"

case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) say "note: ${INSTALL_DIR} isn't on your PATH. Add it, e.g.: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac

"${INSTALL_DIR}/keep-at" version
