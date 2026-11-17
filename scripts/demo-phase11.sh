#!/usr/bin/env bash
# Phase 11 demo: the rules as behaviour.
#
# What Claude Code runs around its tools is run here by hand, with what it
# says to a hook and what the hook answers: no agent is needed to see it.
#
# 1. `kito init` writes the hooks into the project.
# 2. A team works through lanes: each change has a reason.
# 3. A session starts: the agent is told the rules and who is on what.
# 4. An edit in the main directory is not made.
# 5. A lane is told why what it declares is the way it is.
# 6. An edit outside its scope is not made; declared, it is.
# 7. A lane that declared a whole directory was told nothing: its edit is
#    held back once, with the reasons behind the lines it replaces.
# 8. A shell command is checked after it ran.
# 9. Without hooks (any agent, any model): the reasons behind what a lane
#    replaced are told before it is integrated.
#
# Needs python3 (to print JSON and to speak to the MCP server).
#
#   cargo build --release -p kitod -p kito-cli -p kito-mcp
#   scripts/demo-phase11.sh
set -euo pipefail

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-ada}"
DEMO="$(mktemp -d)"
SHOP="$DEMO/shop"
stop() {
  kito --repo "$SHOP" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT
mkdir -p "$SHOP/src"
cd "$SHOP"

run() { printf '\n$ %s\n' "$*"; "$@"; }
pause() { read -r -t "$1" _ < /dev/zero || true; }
# The directory of the demo is not part of the story.
tidy() { sed "s|$SHOP|/shop|g"; }

# What Claude Code says to a hook before an edit, and what the hook answers.
edit() { # file, the text to replace, the new text
  FILE="$1" OLD="$2" NEW="$3" python3 - <<'PY'
import json, os
print(json.dumps({
    "hook_event_name": "PreToolUse", "tool_name": "Edit", "cwd": os.getcwd(),
    "tool_input": {"file_path": os.environ["FILE"], "old_string": os.environ["OLD"],
                   "new_string": os.environ["NEW"], "replace_all": False},
}))
PY
}
answer() { # prints what a hook answered, as the model reads it
  python3 -c '
import json, sys
text = sys.stdin.read().strip()
if not text:
    print("  (the hook says nothing: the edit is made)")
else:
    said = json.loads(text)["hookSpecificOutput"]
    words = said.get("permissionDecisionReason") or said.get("additionalContext")
    kind = {"deny": "NOT MADE"}.get(said.get("permissionDecision"), "told to the model")
    print(f"  [{kind}]")
    for line in words.splitlines():
        print("  " + line)
' | tidy | fold -s -w 96 | sed 's/^\([^ ]\)/  \1/; s/ *$//'
}
try_edit() { # file, old, new: an agent tries an edit
  printf '\n(agent) Edit %s:  %s  ->  %s\n' "${1#"$SHOP"/}" "$2" "$3"
  edit "$1" "$2" "$3" | kito hook before-edit | answer
}

cat > src/shop.py <<'PY'
def total(prices):
    result = 0
    for price in prices:
        result += price
    return result


def discount(amount, percent):
    cut = amount * percent / 100
    return amount - cut
PY
cat > src/billing.py <<'PY'
from shop import total


def invoice(prices):
    return "to pay: " + str(total(prices))
PY
printf '# Shop\n\nAdds up what a customer buys.\n' > README.md

printf '# 1. A project is set up: the rules are hooks, not only words\n'
run kito init | tidy
printf '\n$ the hooks in .claude/settings.json\n'
python3 - <<'PY'
import json
for event, groups in json.load(open(".claude/settings.json"))["hooks"].items():
    for group in groups:
        print(f"  {event:<13} {group.get('matcher', '(every session)'):<36} {group['hooks'][0]['command']}")
PY
kito checkpoint -m "a shop that adds up prices" > /dev/null

printf '\n\n# 2. The team works: each lane says why\n'
kito lane open agent-a --intent "prices in cents" --task "task #7" --scope src/shop.py:total src/billing.py > /dev/null
W="$SHOP/.kito/lanes/1/work"
sed -i 's/        result += price/        result += round(price * 100)/' "$W/src/shop.py"
sed -i 's/str(total(prices))/str(total(prices) \/ 100)/' "$W/src/billing.py"
kito integrate 1 > /dev/null; kito lane close 1 > /dev/null
kito lane open agent-b --intent "whole cents off, never a fraction" --scope src/shop.py:discount > /dev/null
W="$SHOP/.kito/lanes/2/work"
sed -i 's/    cut = amount \* percent \/ 100/    cut = amount * percent \/\/ 100/' "$W/src/shop.py"
kito integrate 2 > /dev/null; kito lane close 2 > /dev/null
run kito log -n 4

printf '\n\n# 3. A session starts: what the agent knows before its first word\n'
printf '\n$ kito hook session-start\n'
printf '{"hook_event_name":"SessionStart","source":"startup","cwd":"%s"}' "$SHOP" | kito hook session-start | answer

printf '\n\n# 4. An edit in the main directory is not made\n'
try_edit "$SHOP/src/shop.py" "    cut = amount * percent // 100" "    cut = percent"

printf '\n\n# 5. The agent opens a lane: it is told why what it declares is the way it is\n'
run kito lane open agent-c --intent "a flat discount" --scope src/shop.py:discount | tidy
W="$SHOP/.kito/lanes/3/work"
try_edit "$W/src/shop.py" "    cut = amount * percent // 100" "    cut = percent"
sed -i 's/    cut = amount \* percent \/\/ 100/    cut = percent/' "$W/src/shop.py"

printf '\n\n# 6. An edit outside what the lane declared is not made; declared, it is\n'
try_edit "$W/src/shop.py" "        result += round(price * 100)" "        result += int(price * 100)"
run kito lane scope 3 --add src/shop.py:total
try_edit "$W/src/shop.py" "        result += round(price * 100)" "        result += int(price * 100)"

printf '\n\n# 7. A lane on a whole directory was told nothing: its edit is held back once\n'
run kito lane open agent-d --intent "friendlier invoices" --scope src/ | tidy
V="$SHOP/.kito/lanes/4/work"
try_edit "$V/src/billing.py" '"to pay: "' '"total due: "'
printf '\n(the same edit, made again)'
try_edit "$V/src/billing.py" '"to pay: "' '"total due: "'
sed -i 's/"to pay: "/"total due: "/' "$V/src/billing.py"

printf '\n\n# 8. A shell command is checked after it ran\n'
pause 0.5
printf '\n(agent) Bash: sed -i ... on README.md in the main directory and in the lane (which declared src/)\n'
sed -i 's/Adds up/Sums/' "$SHOP/README.md"
sed -i 's/Adds up/Sums/' "$V/README.md"
kito sync > /dev/null
printf '{"hook_event_name":"PostToolUse","tool_name":"Bash","cwd":"%s","duration_ms":30,"tool_input":{"command":"sed -i s/Adds\\\\ up/Sums/ %s/README.md %s/README.md"}}' "$SHOP" "$SHOP" "$V" \
  | kito hook after-shell | answer
printf '# Shop\n\nAdds up what a customer buys.\n' > "$SHOP/README.md"
printf '# Shop\n\nAdds up what a customer buys.\n' > "$V/README.md"

printf '\n\n# 9. Without hooks: the reasons are told before the integration\n'
printf '\n(an agent with no hooks changed `discount` in lane/4 through a shell)\n'
sed -i 's/    cut = amount \* percent \/\/ 100/    cut = round(amount * percent \/ 100)/' "$V/src/shop.py"
cat > "$DEMO/mcp.py" <<'PY'
"""Calls one tool of kito-mcp, as an agent's client does, and prints the answer."""
import json, subprocess, sys

server = subprocess.Popen(["kito-mcp", "--repo", sys.argv[1]], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

def ask(method, params, number=[0]):
    number[0] += 1
    server.stdin.write(json.dumps({"jsonrpc": "2.0", "id": number[0], "method": method, "params": params}) + "\n")
    server.stdin.flush()
    return json.loads(server.stdout.readline())["result"]

ask("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "demo", "version": "0"}})
server.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
tools = {tool["name"].replace("kito.", "").replace("kito_", ""): tool["name"] for tool in ask("tools/list", {})["tools"]}

def tool(name, arguments):
    return json.loads(ask("tools/call", {"name": tools[name], "arguments": arguments})["content"][0]["text"])

answer = tool("request_integration", {"lane": sys.argv[2]})
if "job" in answer:
    answer = tool("integration_result", {"job": answer["job"]})
print("  result:", answer["result"], answer.get("checkpoint") or "")
for line in (answer.get("message") or "").splitlines():
    print("  " + line)
server.terminate()
PY
printf '\n(agent) request_integration lane/4\n'
python3 "$DEMO/mcp.py" "$SHOP" lane/4 | fold -s -w 96 | sed 's/^\([^ ]\)/  \1/; s/ *$//'
printf '\n(agent) request_integration lane/4, again\n'
python3 "$DEMO/mcp.py" "$SHOP" lane/4 | sed 's/ *$//'

printf '\n\n# What somebody who watches the repository saw\n'
printf '\n$ kito watch --past | grep "edit refused"\n'
(timeout 1 kito watch --past || true) | grep 'edit refused' | cut -c11-
