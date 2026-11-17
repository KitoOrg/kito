#!/usr/bin/env bash
# Phase 10 demo: the why graph.
#
# 1. A server takes in a repository that already has a history in git.
# 2. A team works on it through lanes: alice's agent puts the prices in
#    cents and then refuses negative ones (two reasons, one task); bob's
#    agent floors the discount. Bob may not integrate: ada lets his lane in.
# 3. Dave's agent starts on `total`. Carol's is asked to change it too, and
#    asks first: why is it the way it is?
# 4. Why is this one line the way it is?
# 5. A file that is not code has reasons too, and they go back to git.
# 6. Earlier work, found by the words of its reason.
# 7. The same answer as a diagram.
#
# Needs python3 (the check of the demo repository) and git.
#
#   cargo build --release -p kitod -p kito-cli -p kito-server
#   scripts/demo-phase10.sh
set -euo pipefail

# Kito alone: `kito connect` would also set the directory up for Claude Code.
export KITO_NO_CLAUDE=1

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-the server}"
DEMO="$(mktemp -d)"
SERVER="$DEMO/server"
stop() {
  for dir in ada bob carol dave; do kito --repo "$DEMO/$dir" daemon stop >/dev/null 2>&1 || true; done
  [ -n "${SERVING:-}" ] && kill "$SERVING" 2>/dev/null || true
  kito --repo "$SERVER/repos/shop" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT
cd "$DEMO"

run() { printf '\n$ %s\n' "$*"; "$@"; }
pause() { read -r -t "$1" _ < /dev/zero || true; }
as() { local who="$1"; shift; (cd "$DEMO/$who" && printf '\n(%s) $ %s\n' "$who" "$*" && "$@"); }
quiet() { local who="$1"; shift; (cd "$DEMO/$who" && "$@" > /dev/null); }
commit() { git -C "$DEMO/shop-src" add -A && git -C "$DEMO/shop-src" -c user.name=Grace -c user.email=grace@example.com commit -q -m "$1"; }

# The repository before Kito: three commits by Grace.
mkdir -p "$DEMO/shop-src/src"
cat > "$DEMO/shop-src/src/shop.py" <<'PY'
def total(prices):
    result = 0
    for price in prices:
        result += price
    return result


def discount(amount, percent):
    cut = amount * percent / 100
    return amount - cut
PY
cat > "$DEMO/shop-src/src/billing.py" <<'PY'
from shop import total


def invoice(prices):
    return "to pay: " + str(total(prices))
PY
printf '# Shop\n\nAdds up what a customer buys.\n' > "$DEMO/shop-src/README.md"
git -C "$DEMO/shop-src" init -q -b main
commit "a shop that adds up prices"
printf '\nPrices are in euros.\n' >> "$DEMO/shop-src/README.md"
commit "say which currency"
sed -i 's/    return amount - cut/    return max(amount - cut, 0)/' "$DEMO/shop-src/src/shop.py"
commit "a discount never goes below zero"

printf '# 1. A repository with a history of its own comes to the server\n'
run git -C "$DEMO/shop-src" log --format='%h %an: %s'
kito-server init "$SERVER" > /dev/null
run kito-server --dir "$SERVER" repo add shop --from "$DEMO/shop-src"
run kito-server --dir "$SERVER" repo set shop --check "python3 -m py_compile src/shop.py src/billing.py"
ADA="$(kito-server --dir "$SERVER" user add ada | grep '^kito_')"
BOB="$(kito-server --dir "$SERVER" user add bob --role write | grep '^kito_')"
CAROL="$(kito-server --dir "$SERVER" user add carol | grep '^kito_')"
DAVE="$(kito-server --dir "$SERVER" user add dave | grep '^kito_')"
kito-server --dir "$SERVER" serve --listen 127.0.0.1:0 > "$DEMO/serving" 2>/dev/null &
SERVING=$!
until [ -s "$DEMO/serving" ]; do pause 0.05; done
ADDR="$(sed -n 's/^kito-server: listening on \([^,]*\),.*/\1/p' "$DEMO/serving")"
for who in ada:$ADA bob:$BOB carol:$CAROL dave:$DAVE; do
  kito connect "$ADDR" shop "${who%%:*}" --token "${who#*:}" > /dev/null
done
printf '\n(ada, bob, carol and dave each connect a directory; bob may write, the others may integrate)\n'

printf '\n\n# 2. The team works: each lane says why\n'
as ada kito lane open agent-a --intent "prices in cents" --task "task #7" --scope src/shop.py:total src/billing.py README.md | head -3
W="$DEMO/ada/.kito/lanes/1/work"
sed -i 's/        result += price/        result += round(price * 100)/' "$W/src/shop.py"
sed -i 's/str(total(prices))/str(total(prices) \/ 100)/' "$W/src/billing.py"
sed -i 's/Prices are in euros./Prices are in euros; totals are kept in cents./' "$W/README.md"
quiet ada kito sync 1
as ada kito intent "no negative prices" --lane 1
sed -i 's/    result = 0/    result = 0\n    if any(price < 0 for price in prices):\n        raise ValueError("negative price")/' "$W/src/shop.py"
as ada kito integrate 1
quiet ada kito lane close 1

as bob kito lane open agent-b --intent "whole cents off, never a fraction" --scope src/shop.py:discount | head -3
W="$DEMO/bob/.kito/lanes/2/work"
sed -i 's/    cut = amount \* percent \/ 100/    cut = amount * percent \/\/ 100/' "$W/src/shop.py"
as bob kito integrate 2
as ada kito integrate 2
quiet bob kito lane close 2

printf '\n\n# 3. Before changing `total`, carol'"'"'s agent asks why it is the way it is\n'
as dave kito lane open agent-d --intent "totals for several currencies" --scope src/shop.py:total | head -3
as carol kito why src/shop.py:total

printf '\n\n# 4. Why is this line the way it is?\n'
LINE="$(grep -n 'cut = ' "$DEMO/carol/src/shop.py" | cut -d: -f1)"
as carol kito why "src/shop.py:$LINE"

printf '\n\n# 5. A file that is not code\n'
as carol kito why README.md

printf '\n\n# 6. Earlier work, by the words of its reason\n'
as carol kito reasons cents
as carol kito reasons "task #7"

printf '\n\n# 7. The same answer as a diagram (Mermaid)\n'
as carol kito why src/shop.py:total --graph
