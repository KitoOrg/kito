#!/usr/bin/env bash
# Phase 22 demo: a registry of plugins.
#
# The plugin is `kito-hello`, the smallest there is (an example of
# kito-cli), and the registry is a directory of files made here: what
# kito.sh/registry is, without the network.
#
# 1. What the registry has, and a plugin installed by its name.
# 2. A file that is not the one the registry listed installs nothing.
# 3. A later version: outdated, update.
# 4. Taking it out: it cleans up after itself, and goes.
# 5. On a server: an administrator installs, enables and removes one
#    over HTTP, and nobody else does.
#
#   cargo build --release -p kitod -p kito-cli -p kito-server
#   cargo build --release -p kito-cli --example kito-hello
#   scripts/demo-phase22.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-ada}"
DEMO="$(mktemp -d)"
APP="$DEMO/app"
SERVER="$DEMO/server"
REGISTRY="$DEMO/registry"
export KITO_CONFIG_DIR="$DEMO/config"
export KITO_REGISTRY="file://$REGISTRY"
SERVING=""
stop() {
  [ -n "$SERVING" ] && kill "$SERVING" 2>/dev/null || true
  kito --repo "$APP" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT

# The demo's own directories, ports and secrets are not part of what it shows.
tidy() {
  sed -e "s#$DEMO#/demo#g" -e "s#127\.0\.0\.1:[0-9]*#127.0.0.1:PORT#g" \
    -e "s#kito_[0-9a-f]\{16,\}#kito_…#g" -e "s#kitos_[0-9a-f]*#kitos_…#g" -e "s#[0-9a-f]\{64\}#SHA256#g"
}
run() { printf '\n$ %s\n' "$*" | tidy; "$@" 2>&1 | tidy; }
refused() { printf '\n$ %s\n' "$*" | tidy; "$@" 2>&1 | tidy || true; }

PLATFORM="$(uname -s | tr '[:upper:]' '[:lower:]' | sed 's/darwin/macos/')-$(uname -m)"
[ "${PLATFORM%%-*}" = macos ] && PLATFORM="${PLATFORM/aarch64/arm64}"

# One version of the plugin, packed as its author would and listed.
pack() {
  local version="$1"
  rm -rf "$DEMO/pack" && mkdir -p "$DEMO/pack/kito-hello" "$REGISTRY/plugins"
  cp "$BIN/examples/kito-hello" "$DEMO/pack/kito-hello/"
  cat > "$DEMO/pack/kito-hello/kito-plugin.toml" <<TOML
name = "hello"
version = "$version"
description = "Says what it is told"
api = 2
permissions = ["read", "server_events"]

[[command]]
name = "hello"
about = "Says what it is told"

[assistant]
name = "hello"

[cleanup]
about = "Takes HELLO.md out of the project"
TOML
  tar -czf "$REGISTRY/kito-hello-$version.tar.gz" -C "$DEMO/pack" kito-hello
  sum="$(sha256sum "$REGISTRY/kito-hello-$version.tar.gz" | cut -d' ' -f1)"
  LISTED="${LISTED:+$LISTED,}\"$version\":{\"api\":2,\"files\":{\"$PLATFORM\":{\"url\":\"kito-hello-$version.tar.gz\",\"sha256\":\"${2:-$sum}\"}}}"
  printf '{"name":"hello","about":"Says what it is told","versions":{%s}}\n' "$LISTED" > "$REGISTRY/plugins/hello.json"
  printf '{"registry":1,"plugins":{"hello":{"about":"Says what it is told","latest":"%s"}}}\n' "$version" > "$REGISTRY/index.json"
}
LISTED=""
pack 0.9.0

mkdir -p "$APP" && cd "$APP"
printf 'export const price = 1;\n' > shop.ts
kito init >/dev/null

echo "== 1. Installed by its name"
run kito plugin search
run kito plugin add hello
run kito plugin enable hello
run kito hello env

echo
echo "== 2. A file that is not the one the registry listed"
GOOD="$LISTED"; LISTED=""
pack 0.9.0 0000000000000000000000000000000000000000000000000000000000000000
refused kito plugin add hello
LISTED="$GOOD"

echo
echo "== 3. A later version"
pack 0.10.0
run kito plugin outdated
run kito plugin update

echo
echo "== 4. Taking it out"
run kito assistant add hello
run ls HELLO.md
run kito plugin disable hello --purge
refused ls HELLO.md
run kito plugin remove hello

echo
echo "== 5. A server's plugins, over HTTP, for its administrators"
kito-server init "$SERVER" >/dev/null
kito-server --dir "$SERVER" repo add shop >/dev/null
ADA="$(kito-server --dir "$SERVER" user add ada | grep '^kito_')"
BOB="$(kito-server --dir "$SERVER" user add bob | grep '^kito_')"
run kito-server --dir "$SERVER" user admin ada
kito-server --dir "$SERVER" serve --plain --listen 127.0.0.1:0 --http 127.0.0.1:0 > "$DEMO/serving.log" 2>&1 &
SERVING=$!
for _ in $(seq 1 100); do grep -q '^kito-server: http' "$DEMO/serving.log" && break; sleep 0.1; done
HTTP="$(sed -n 's#^kito-server: \(http://[^,]*\),.*#\1#p' "$DEMO/serving.log")"
ask() { curl -s -H "Authorization: Bearer $1" -H 'Content-Type: application/json' "${@:2}" "$HTTP/-/admin/plugins"; echo; }
printf '\n$ curl -H "Authorization: Bearer <bob>" …/-/admin/plugins\n'
ask "$BOB" | tidy
printf '\n$ curl -H "Authorization: Bearer <ada>" -d %s …/-/admin/plugins\n' "'{\"action\":\"install\",\"name\":\"hello\"}'"
ask "$ADA" -d '{"action":"install","name":"hello"}' | tidy
printf '\n$ … -d %s\n' "'{\"action\":\"install\",\"name\":\"https://evil.example/x\"}'"
ask "$ADA" -d '{"action":"install","name":"https://evil.example/x"}' | tidy
printf '\n$ … -d %s\n' "'{\"action\":\"enable\",\"name\":\"hello\",\"permissions\":[\"server_events\"]}'"
ask "$ADA" -d '{"action":"enable","name":"hello","permissions":["server_events"]}' | tidy
run kito-server --dir "$SERVER" plugin list
printf '\n$ … -d %s\n' "'{\"action\":\"disable\",\"name\":\"hello\",\"purge\":true}'"
ask "$ADA" -d '{"action":"disable","name":"hello","purge":true}' | tidy
printf '\n$ … -d %s\n' "'{\"action\":\"remove\",\"name\":\"hello\"}'"
ask "$ADA" -d '{"action":"remove","name":"hello"}' | tidy
run kito-server --dir "$SERVER" plugin list
