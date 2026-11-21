#!/usr/bin/env bash
# Phase 28 demo: a role in each repository, and one on the server.
#
# 1. People and what they may use, on the server's machine.
# 2. A maintainer, from a directory connected to the repository.
# 3. An admin, from a connected directory.
#
#   cargo build --release -p kitod -p kito-cli -p kito-server
#   scripts/demo-phase28.sh
set -euo pipefail

BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
DEMO="$(mktemp -d)"
export HOME="$DEMO/home" KITO_CONFIG_DIR="$DEMO/config" KITO_AUTHOR=ada KITO_NO_CLAUDE=1 KITOD_BIN="$BIN/kitod"
mkdir -p "$HOME"
export PATH="$BIN:/usr/bin:/bin"
SERVER="$DEMO/server"
PID=""
trap 'kill $PID 2>/dev/null || true; for d in "$DEMO"/*-desk "$SERVER"/repos/*; do kito --repo "$d" daemon stop >/dev/null 2>&1 || true; done; rm -rf "$DEMO"' EXIT

tidy() { sed -e "s#$DEMO#/demo#g" -e 's/kito_[0-9a-f]\{64\}/kito_…/g' -e 's/sha256:[0-9a-f]\{64\}/sha256:…/g' -e 's/127\.0\.0\.1:[0-9]*/127.0.0.1:7421/g'; }
run() { printf '\n$ %s\n' "$*" | tidy; "$@" 2>&1 | tidy || true; }
token() { "$@" | grep '^kito_'; }

echo "== 1. People and what they may use"
kito-server init "$SERVER" > /dev/null
kito-server --dir "$SERVER" repo add shop > /dev/null
kito-server --dir "$SERVER" repo add site > /dev/null
ERIN=$(token kito-server --dir "$SERVER" user add erin --repo shop:maintain)
BOB=$(token kito-server --dir "$SERVER" user add bob --repo shop)
ADA=$(token kito-server --dir "$SERVER" user add ada --role admin)
run kito-server --dir "$SERVER" user add carol --role write
run kito-server --dir "$SERVER" grant bob site read
run kito-server --dir "$SERVER" user list

kito-server --dir "$SERVER" serve --plain --listen 127.0.0.1:0 > "$DEMO/serving" 2>&1 &
PID=$!
for _ in $(seq 50); do grep -q listening "$DEMO/serving" && break; sleep 0.1; done
ADDR=$(sed -n 's/.*listening on \([^,]*\),.*/\1/p' "$DEMO/serving")

echo
echo "== 2. A maintainer, from a connected directory"
cd "$DEMO"
kito connect "$ADDR/shop" erin-desk --token "$ERIN" --plain > /dev/null
cd erin-desk
run kito members
run kito grant bob integrate
run kito people

echo
echo "== 3. An admin, from a connected directory"
cd "$DEMO"
kito connect "$ADDR/site" ada-desk --token "$ADA" --plain > /dev/null
cd ada-desk
run kito people add dave
run kito people role dave owner
run kito people
