#!/usr/bin/env bash
# Phase 21 demo: a team's kitchen.
#
# The kitchen is the plugin `kito-kitchen` (a repository of its own):
# build it there with `scripts/build.sh`, and say where it is in KITCHEN
# (default: ../kito-kitchen/plugin).
#
# 1. A repository of the server is the kitchen; `shop` picks a part, and
#    the server lets the plugin run in the directories connected to it.
# 2. Ada connects a directory: the kitchen is there, for her.
# 3. Her agent is told when a session starts, after Kito's own rules.
# 4. A tool of the kitchen runs in the lane the agent works in.
# 5. The kitchen changes for everybody when a change to it is integrated.
# 6. The server away, the kitchen is the one it last gave.
#
#   cargo build --release -p kitod -p kito-cli -p kito-server
#   scripts/demo-phase21.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
HERE="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${KITO_BIN:-$HERE/target/release}"
KITCHEN="${KITCHEN:-$HERE/../kito-kitchen/plugin}"
export PATH="$BIN:$PATH"
DEMO="$(mktemp -d)"
SERVER="$DEMO/server"
DESK="$DEMO/desk"
SERVING=""
# The server's user and ada each have their own plugins.
theirs() { KITO_AUTHOR="the server" KITO_CONFIG_DIR="$DEMO/server-config" "$@"; }
mine() { KITO_AUTHOR=ada KITO_CONFIG_DIR="$DEMO/ada-config" "$@"; }
stop() {
  [ -n "$SERVING" ] && kill "$SERVING" 2>/dev/null || true
  mine kito --repo "$DESK" daemon stop >/dev/null 2>&1 || true
  for repo in kitchen shop; do theirs kito --repo "$SERVER/repos/$repo" daemon stop >/dev/null 2>&1 || true; done
  rm -rf "$DEMO"
}
trap stop EXIT

# The demo's own directories, ports and secrets are not part of what it shows.
tidy() { sed -e "s#$DEMO#/demo#g" -e "s#127\.0\.0\.1:[0-9]*#127.0.0.1:PORT#g" -e "s#kito_[0-9a-f]\{16,\}#kito_…#g"; }
run() { printf '\n$ %s\n' "$*" | sed -e 's/^\$ theirs /$ /' -e 's/^\$ mine /$ /' | tidy; "$@" 2>&1 | tidy; }

theirs kito-server init "$SERVER" >/dev/null
theirs kito-server --dir "$SERVER" repo add kitchen >/dev/null
theirs kito-server --dir "$SERVER" repo add shop >/dev/null
K="$SERVER/repos/kitchen"
S="$SERVER/repos/shop"

# The kitchen's files, and the shop's.
mkdir -p "$K/concepts" "$K/recipes" "$K/tools" "$K/parts/typescript/concepts" "$S/src"
printf '.kito/\n' | tee "$K/.gitignore" > "$S/.gitignore"
cat > "$K/kitchen.toml" <<'EOF'
name = "Shopworks"
about = "How we work on the shop."
EOF
cat > "$K/behaviour.md" <<'EOF'
Say what you are about to do in one sentence before you do it.
Run the tests of what you changed (`run`, the tool `test`) before you ask for an integration.
EOF
printf '# Amounts are whole cents\n\nNever a float: half a cent is lost.\n' > "$K/concepts/money.md"
printf '# Amounts are of the type `Cents`\n\n`type Cents = number`, always an integer. See src/money.ts.\n' > "$K/parts/typescript/concepts/money.md"
printf '# How a release is made\n\n1. Integrate what goes in.\n2. Tag the checkpoint.\n3. Tell the channel.\n' > "$K/recipes/release.md"
cat > "$K/tools/test.sh" <<'EOF'
#!/bin/sh
echo "testing ${1:-everything} in $(basename "$PWD") of ${KITO_LANE}"
grep -c export src/*.ts
EOF
cat > "$K/tools/test.toml" <<'EOF'
about = "Runs the tests of the shop; `only` names one file"
script = "test.sh"
args = ["{only}"]

[input]
type = "object"
required = ["only"]
EOF
(cd "$K" && theirs kito checkpoint -m "the kitchen" >/dev/null)
printf 'export function total(prices: number[]): number {\n  return prices.reduce((sum, price) => sum + price, 0);\n}\n' > "$S/src/shop.ts"
(cd "$S" && theirs kito checkpoint -m "first version" >/dev/null)

echo "== 1. A repository of the server is the kitchen"
theirs kito-server --dir "$SERVER" plugin add --path "$KITCHEN" >/dev/null
run theirs kito-server --dir "$SERVER" plugin enable kitchen --for-clients
run theirs kito-server --dir "$SERVER" kitchen set kitchen
run theirs kito-server --dir "$SERVER" kitchen parts shop typescript
run theirs kito-server --dir "$SERVER" kitchen check

TOKEN="$(theirs kito-server --dir "$SERVER" user add ada --role write | grep '^kito_')"
# Started as itself, not through `theirs`: it is what is stopped at the end.
KITO_AUTHOR="the server" KITO_CONFIG_DIR="$DEMO/server-config" \
  kito-server --dir "$SERVER" serve --plain --listen 127.0.0.1:0 > "$DEMO/serving.log" 2>&1 &
SERVING=$!
for _ in $(seq 1 100); do grep -q '^kito-server: listening' "$DEMO/serving.log" && break; sleep 0.1; done
ADDRESS="$(sed -n 's/^kito-server: listening on \([^,]*\),.*/\1/p' "$DEMO/serving.log")"

echo
echo "== 2. Ada connects a directory, and has the plugin"
mine kito connect "$ADDRESS" shop "$DESK" --plain --token "$TOKEN" >/dev/null
mine kito plugin add --path "$KITCHEN" >/dev/null
run mine kito --repo "$DESK" kitchen
run mine kito --repo "$DESK" kitchen concept money

echo
echo "== 3. Her agent is told when a session starts, after Kito's own rules"
printf '\n$ kito hook session-start   (what the assistant runs)\n'
printf '{"hook_event_name":"SessionStart","cwd":"%s"}' "$DESK" \
  | (cd "$DESK" && mine kito hook session-start) \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["hookSpecificOutput"]["additionalContext"])' \
  | sed -n '/From the plugin/,$p' | tidy

echo
echo "== 4. A tool of the kitchen runs in the lane the agent works in"
(cd "$DESK" && mine kito lane open alice --intent "prices in cents" --scope src/shop.ts >/dev/null)
printf '\n$ the kitchen tool `run`: {"name": "test", "lane": "lane/1", "args": {"only": "shop; rm -rf ."}}\n'
printf '{"name":"test","lane":"lane/1","args":{"only":"shop; rm -rf ."}}' \
  | (cd "$DESK" && mine kito kitchen tool run) | tidy
echo

echo
echo "== 5. Integrated into the kitchen, it is the kitchen for everybody"
printf '# An order is what a customer paid for\n\nNot a basket: a basket is not paid yet.\n' > "$K/concepts/order.md"
(cd "$K" && theirs kito checkpoint -m "what an order is" >/dev/null)
run mine kito --repo "$DESK" kitchen concept order

echo
echo "== 6. The server away, the kitchen is the one it last gave"
kill "$SERVING"; wait "$SERVING" 2>/dev/null || true; SERVING=""
run mine kito --repo "$DESK" kitchen
