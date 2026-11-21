#!/usr/bin/env bash
# Phase 19 demo: plugins.
#
# The plugin is `kito-hello`, the smallest there is (an example of
# kito-cli): a command, a gate of integration, and events.
#
# 1. Installed for a person, it runs nowhere; a repository enables it.
# 2. Its command is a command of Kito there.
# 3. The daemon answers it what it was given, and refuses the rest.
# 4. Its gate is asked before a lane is integrated: fail, wait, pass.
# 5. It is run for the events it listed.
#
#   cargo build --release -p kitod -p kito-cli
#   cargo build --release -p kito-cli --example kito-hello
#   scripts/demo-phase19.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-ada}"
DEMO="$(mktemp -d)"
APP="$DEMO/app"
# The person's own Kito directory, for the demo: plugins are installed there.
export KITO_CONFIG_DIR="$DEMO/config"
stop() {
  kito --repo "$APP" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT

run() { printf '\n$ %s\n' "$*"; "$@"; }
# A command that is expected to be refused.
refused() { printf '\n$ %s\n' "$*"; "$@" 2>&1 || true; }

# The plugin as a download would leave it: its program and its manifest.
mkdir -p "$DEMO/kito-hello"
cp "$BIN/examples/kito-hello" "$DEMO/kito-hello/"
cat > "$DEMO/kito-hello/kito-plugin.toml" <<'TOML'
name = "hello"
version = "0.1.0"
description = "Says what it is told"
api = 1
permissions = ["read", "write"]

[[command]]
name = "hello"
about = "Ask the daemon as a plugin"

[[gate]]
name = "prices"
about = "No price under 0.5"

[[event]]
kinds = ["lane_opened", "integrated"]
TOML

mkdir -p "$APP"; cd "$APP"
kito init >/dev/null
printf '.kito/\n' > .gitignore
printf 'export const price = 1;\n' > shop.ts
kito checkpoint -m "first version" >/dev/null

run kito plugin add --path "$DEMO/kito-hello" | sed "s|$DEMO|<demo>|"
refused kito hello env
run kito plugin enable hello
run kito hello env | sed "s|$DEMO|<demo>|"
run kito hello ask lanes
refused kito hello ask configure '{"checks":["true"]}'

kito lane open alice --intent "a cheaper price" --scope ./ >/dev/null
WORK="$APP/.kito/lanes/1/work"
printf 'export const price = 0.25;\n' > "$WORK/shop.ts"
printf 'prices under 0.5 are not allowed\n' > "$WORK/GATE_FAIL"
refused kito integrate 1
rm "$WORK/GATE_FAIL"
printf 'the ticket SHOP-12 is not approved yet\n' > "$WORK/GATE_WAIT"
run kito integrate 1
rm "$WORK/GATE_WAIT"
run kito integrate 1 | sed -E 's/\(git [0-9a-f]+\)/(git …)/'

# The events it was run for, in the file it keeps.
for _ in $(seq 1 100); do
  grep -q '^integrated' .kito/plugins/hello/events.log 2>/dev/null && break
  sleep 0.1
done
run cat .kito/plugins/hello/events.log
echo; echo '$ kito status'
kito status | grep Plugin
