#!/usr/bin/env bash
# Phase 24 demo: a lane says which assistant works in it, and on what model.
#
# 1. A program that speaks MCP opens a lane: it named itself when it
#    connected, and its agent says the model it is.
# 2. Claude Code says its model when a session starts; the lane its next
#    edit is in says both.
# 3. Another assistant takes a lane over: what was known of the one
#    before goes with it.
# 4. A lane nobody said anything of says nothing.
#
#   cargo build --release -p kitod -p kito-cli -p kito-mcp
#   scripts/demo-phase24.sh
set -euo pipefail

export KITO_NO_CLAUDE=1 KITO_AUTHOR=ada
unset CLAUDECODE
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
export PATH="$BIN:$PATH"
DEMO="$(mktemp -d)"
export KITO_CONFIG_DIR="$DEMO/config"
APP="$DEMO/shop"
trap 'kito --repo "$APP" daemon stop >/dev/null 2>&1 || true; rm -rf "$DEMO"' EXIT

tidy() { sed -e "s#$DEMO#/demo#g"; }
run() { printf '\n$ %s\n' "$*" | tidy; "$@" 2>&1 | tidy; }
say() { printf '\n%s\n' "$*"; }

mkdir -p "$APP/src" && cd "$APP"
kito init >/dev/null
printf 'export function total(prices: number[]): number {\n  return prices.reduce((sum, price) => sum + price, 0);\n}\n' > src/shop.ts
printf '# Shop\n' > README.md
printf '.kito/\n' > .gitignore
kito checkpoint -m "first version" >/dev/null

echo "== 1. Through the MCP server: the program names itself, the agent says its model"
say '(a program connects to kito-mcp as "claude-code" 2.1.0, and calls open_lane with model "claude-opus-5")'
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"claude-code","version":"2.1.0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"kito.open_lane","arguments":{"agent":"alice","intent":"prices in cents","scope":["src/shop.ts:total"],"model":"claude-opus-5"}}}'
  sleep 2
} | kito-mcp --repo "$APP" >/dev/null
run kito lanes

echo
echo "== 2. Claude Code's hooks: the model when a session starts, the session at each edit"
kito lane open bob --intent "say what the shop sells" --scope README.md >/dev/null
run kito lanes
say '(a session of Claude Code starts on "claude-sonnet-5", then edits README.md in lane/2)'
printf '{"hook_event_name":"SessionStart","session_id":"session-7","cwd":"%s","model":"claude-sonnet-5"}' "$APP" \
  | CLAUDECODE=1 kito hook session-start >/dev/null
printf '{"hook_event_name":"PreToolUse","tool_name":"Edit","session_id":"session-7","cwd":"%s","tool_input":{"file_path":"%s","old_string":"# Shop","new_string":"# The shop"}}' \
  "$APP" "$APP/.kito/lanes/2/work/README.md" | CLAUDECODE=1 kito hook before-edit
run kito board

echo
echo "== 3. Another assistant takes lane/2 over"
run kito hook before-edit --file "$APP/.kito/lanes/2/work/README.md" --assistant codex --model gpt-5-codex
say '$ kito --json lanes     (the part that says it)'
kito --json lanes | tr -d '\n ' | grep -o '"id":"[^"]*"\|"assistant":{[^}]*}'

echo
echo "== 4. Opened by hand: said, or not said"
run kito lane open carol --intent "a line about returns" --scope README.md --assistant zcode
kito lane open dave --intent "a line about delivery" --scope README.md >/dev/null
run kito lanes
