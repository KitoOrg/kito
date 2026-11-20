#!/bin/sh
# Installs Kito from a GitHub release: `kito`, `kitod`, `kito-mcp` and
# `kito-server`, in ~/.local/bin unless KITO_INSTALL_DIR says otherwise.
#
#   curl -fsSL https://github.com/KitoOrg/kito/releases/latest/download/install.sh | sh
#
# KITO_VERSION=0.16.0   a version instead of the latest
# KITO_INSTALL_DIR=DIR  where the programs go
# GITHUB_TOKEN=...      to read the releases of a private repository
set -eu

REPO="KitoOrg/kito"
DIR="${KITO_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'kito: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }
need() { command -v "$1" > /dev/null 2>&1 || die "this needs $1"; }

need curl
need tar
need uname

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) die "no build for $(uname -s): Kito runs on Linux and macOS (on Windows, in WSL)" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=$([ "$os" = macos ] && echo arm64 || echo aarch64) ;;
  *) die "no build for $(uname -m)" ;;
esac

api() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/vnd.github+json" "$@"
  else
    curl -fsSL -H "Accept: application/vnd.github+json" "$@"
  fi
}

if [ -n "${KITO_VERSION:-}" ]; then
  release=$(api "https://api.github.com/repos/$REPO/releases/tags/v${KITO_VERSION#v}") \
    || die "no release v${KITO_VERSION#v}"
else
  release=$(api "https://api.github.com/repos/$REPO/releases/latest") \
    || die "no release found (a private repository needs GITHUB_TOKEN)"
fi
version=$(printf '%s' "$release" | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -n 1)
[ -n "$version" ] || die "could not read the release"
name="kito-$version-$os-$arch"

# The API URL of an asset, from the release's JSON: works for private
# repositories too, with the token.
asset_url() {
  # One asset per line: its "url" comes before its "name", and before the
  # object of its uploader starts.
  printf '%s' "$release" | tr -d '\n' | tr '{' '\n' | grep "\"name\": *\"$1\"" \
    | sed -n 's/.*"url": *"\([^"]*\)".*/\1/p' | head -n 1
}
fetch() {
  url=$(asset_url "$1")
  [ -n "$url" ] || die "the release has no $1"
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/octet-stream" -o "$2" "$url"
  else
    curl -fsSL -H "Accept: application/octet-stream" -o "$2" "$url"
  fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
say "downloading $name"
fetch "$name.tar.gz" "$tmp/$name.tar.gz"
fetch "$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256"

expected=$(cut -d' ' -f1 < "$tmp/$name.tar.gz.sha256")
if command -v sha256sum > /dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
else
  need shasum
  actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)
fi
[ "$expected" = "$actual" ] || die "the checksum of $name.tar.gz does not match: not installed"

tar -C "$tmp" -xzf "$tmp/$name.tar.gz"
mkdir -p "$DIR"
for bin in kito kitod kito-mcp kito-server; do
  install -m 755 "$tmp/$name/$bin" "$DIR/$bin"
done
say "Kito $version is in $DIR"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say "add it to your PATH: export PATH=\"$DIR:\$PATH\"" ;;
esac
