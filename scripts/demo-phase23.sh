#!/usr/bin/env bash
# Phase 23 demo: the registry is signed.
#
# 1. A registry's keeper makes a key, once, and signs the registry.
# 2. Somebody who knows the key installs from it: checked.
# 3. A file changed since it was signed, and another's key: refused.
# 4. An old index, signed as it was, served again: refused.
#
#   cargo build --release -p kitod -p kito-cli
#   cargo build --release -p kito-cli --example kito-hello
#   scripts/demo-phase23.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
export PATH="$BIN:$PATH"
DEMO="$(mktemp -d)"
REGISTRY="$DEMO/registry"
export KITO_CONFIG_DIR="$DEMO/config"
export KITO_REGISTRY="file://$REGISTRY"
trap 'rm -rf "$DEMO"' EXIT

# The demo's own directories and the keys it makes are not part of what it shows.
tidy() { sed -e "s#$DEMO#/demo#g" -e "s#[0-9a-f]\{64\}#…#g"; }
run() { printf '\n$ %s\n' "$*" | tidy; "$@" 2>&1 | tidy; }
refused() { printf '\n$ %s\n' "$*" | tidy; "$@" 2>&1 | tidy || true; }

PLATFORM="$(uname -s | tr '[:upper:]' '[:lower:]' | sed 's/darwin/macos/')-$(uname -m)"
[ "${PLATFORM%%-*}" = macos ] && PLATFORM="${PLATFORM/aarch64/arm64}"
mkdir -p "$DEMO/pack/kito-hello" "$REGISTRY/plugins"
cp "$BIN/examples/kito-hello" "$DEMO/pack/kito-hello/"
printf 'name = "hello"\nversion = "0.9.0"\ndescription = "Says what it is told"\napi = 2\npermissions = ["read"]\n' \
  > "$DEMO/pack/kito-hello/kito-plugin.toml"
tar -czf "$REGISTRY/kito-hello-0.9.0.tar.gz" -C "$DEMO/pack" kito-hello
SUM="$(sha256sum "$REGISTRY/kito-hello-0.9.0.tar.gz" | cut -d' ' -f1)"
printf '{"registry":1,"plugins":{"hello":{"about":"Says what it is told","latest":"0.9.0"}}}\n' > "$REGISTRY/index.json"
printf '{"name":"hello","about":"Says what it is told","versions":{"0.9.0":{"api":2,"files":{"%s":{"url":"kito-hello-0.9.0.tar.gz","sha256":"%s"}}}}}\n' \
  "$PLATFORM" "$SUM" > "$REGISTRY/plugins/hello.json"

echo "== 1. A key, once, and the registry signed"
run kito registry key "$DEMO/registry.key"
KEY="$(kito registry sign "$REGISTRY" --key "$DEMO/registry.key" | sed -n 's/^Checked with the key //p')"
run ls "$REGISTRY" "$REGISTRY/plugins"

echo
echo "== 2. Somebody who knows the key"
export KITO_REGISTRY_KEY="$KEY"
printf '\n$ export KITO_REGISTRY_KEY=<the key>\n'
run kito plugin add hello

echo
echo "== 3. Changed since it was signed, and another's key"
cp "$REGISTRY/plugins/hello.json" "$DEMO/good.json"
sed -i.bak "s/$SUM/$(printf '0%.0s' $(seq 1 64))/" "$REGISTRY/plugins/hello.json" && rm "$REGISTRY/plugins/hello.json.bak"
refused kito plugin add hello
cp "$DEMO/good.json" "$REGISTRY/plugins/hello.json"
printf '\n$ KITO_REGISTRY_KEY=<another key> kito plugin search\n'
KITO_REGISTRY_KEY="$(printf 'ab%.0s' $(seq 1 32))" kito plugin search 2>&1 | tidy || true
printf '\n$ KITO_REGISTRY_KEY= kito plugin add hello   (nobody told this person the key)\n'
KITO_REGISTRY_KEY="" kito plugin add hello 2>&1 | tidy | sed -n 3p

echo
echo "== 4. An old index, served again"
cp "$REGISTRY/index.json" "$DEMO/old.json"; cp "$REGISTRY/index.json.sig" "$DEMO/old.sig"
run kito registry sign "$REGISTRY" --key "$DEMO/registry.key"
run kito plugin search
cp "$DEMO/old.json" "$REGISTRY/index.json"; cp "$DEMO/old.sig" "$REGISTRY/index.json.sig"
refused kito plugin search
