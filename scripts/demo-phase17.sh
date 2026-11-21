#!/usr/bin/env bash
# Phase 17 demo: review of a lane.
#
# 1. The diff of a lane: its files, its functions, its lines.
# 2. A comment on a line, and a request for changes: the lane waits.
# 3. The agent in the lane is told before its next edit.
# 4. An answer, the comment resolved, the request taken back.
# 5. Integrating is its own action.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase17.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-ada}"
DEMO="$(mktemp -d)"
APP="$DEMO/app"
stop() {
  kito --repo "$APP" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT

run() { printf '\n$ %s\n' "$*"; "$@"; }

mkdir -p "$APP/src"; cd "$APP"
cat > src/shop.ts <<'TS'
export function total(prices: number[]): number {
  let sum = 0;
  for (const price of prices) {
    sum += price;
  }
  return sum;
}

export function discount(amount: number, percent: number): number {
  return amount - (amount * percent) / 100;
}
TS
kito init >/dev/null
printf '.kito/\n' > .gitignore
kito checkpoint -m "first version" >/dev/null
kito lane open alice --intent "prices in cents" --scope ./ >/dev/null
WORK="$APP/.kito/lanes/1/work"

# The agent works: a body changed, a function renamed.
sed -i.bak 's/sum += price;/sum += Math.floor(price * 100);/' "$WORK/src/shop.ts" && rm "$WORK/src/shop.ts.bak"
kito rename src/shop.ts:discount rebate --lane 1 >/dev/null

run kito lane diff 1
run kito lane comment 1 "Half a cent is lost: round, do not floor." --on src/shop.ts:4
run kito lane request-changes 1 "See the comment on line 4."
run kito integrate 1

printf '\n# The agent goes on. Its next edit is held back once, with what was said:\n'
printf '{"hook_event_name":"PreToolUse","tool_name":"Edit","cwd":"%s","tool_input":{"file_path":"%s/src/shop.ts","old_string":"Math.floor","new_string":"Math.round","replace_all":false}}' "$WORK" "$WORK" \
  | kito hook before-edit \
  | sed -e 's/.*"permissionDecisionReason":"//' -e 's/"[},].*//' -e 's/\\n/\n/g'

sed -i.bak 's/Math.floor/Math.round/' "$WORK/src/shop.ts" && rm "$WORK/src/shop.ts.bak"
run kito lane comment 1 "Rounded now." --reply comment#1
run kito lane comments 1
run kito lane resolve-comment comment#1
run kito lane withdraw 1
run kito integrate 1
