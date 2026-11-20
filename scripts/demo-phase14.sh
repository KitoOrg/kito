#!/usr/bin/env bash
# Phase 14 demo: git both ways.
#
# A repository kept by Kito pushes to a git remote. Grace does not use
# Kito: she clones the remote, commits with plain git and pushes.
#
# 1. Grace pushes two commits while an agent works on the same file.
# 2. Kito brings her commits in through a lane, merges them function by
#    function with the agent's work, and pushes the merge: nothing is
#    forced, and nobody reconciles anything by hand.
# 3. Why her code is the way it is: her commit message, under her name.
# 4. Grace pulls with git and has the agent's work.
# 5. A commit of hers that conflicts with a lane waits until somebody
#    decides; then the two histories are joined.
#
# Needs git.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase14.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-ada}"
DEMO="$(mktemp -d)"
SHOP="$DEMO/shop"
REMOTE="$DEMO/remote.git"
GRACE="$DEMO/grace"
stop() {
  kito --repo "$SHOP" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT

run() { printf '\n$ %s\n' "$*"; "$@"; }
pause() { read -r -t "$1" _ < /dev/zero || true; }
tidy() { sed "s|$DEMO|/demo|g"; }
grace() { git -C "$GRACE" -c user.name=Grace -c user.email=grace@example.com "$@"; }
# Waits until the remote's branch is at a commit that has `$1` behind it.
at_remote() {
  for _ in $(seq 200); do
    if git -C "$REMOTE" merge-base --is-ancestor "$1" main 2>/dev/null; then return 0; fi
    pause 0.05
  done
  echo "the remote never got $1" >&2; return 1
}

mkdir -p "$SHOP/src"; cd "$SHOP"
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
git init -q --bare -b main "$REMOTE"
kito init . > /dev/null
sed -i "s|^# push = \"origin\"|push = \"$REMOTE\"|" .kito/config.toml
kito daemon stop > /dev/null
kito checkpoint -m "a shop that adds up prices" > /dev/null
kito pull > /dev/null
at_remote "$(git rev-parse main)"
git clone -q "$REMOTE" "$GRACE"

printf '# 1. Grace works with plain git, an agent works through Kito\n'
sed -i 's|    cut = amount \* percent / 100|    cut = amount * percent // 100|' "$GRACE/src/shop.py"
grace commit -qam "whole cents off"
printf 'Prices are in euros.\n' > "$GRACE/NOTES.md"
grace add -A; grace commit -qm "say which currency"
grace push -q origin main
printf '\n(grace) $ git log --oneline\n'; grace log --oneline
kito lane open agent-a --intent "prices in cents" --scope src/shop.py:total > /dev/null
sed -i 's|        result += price|        result += round(price * 100)|' "$SHOP/.kito/lanes/1/work/src/shop.py"
run kito integrate 1
kito lane close 1 > /dev/null

printf '\n\n# 2. Her commits are brought in, merged and pushed back\n'
at_remote "$(grace rev-parse HEAD)"; at_remote "$(git rev-parse main)"
run kito log -n 4
run cat src/shop.py
printf '\n$ git log --graph --oneline main     # at the remote\n'
git -C "$REMOTE" log --graph --oneline --format='%h %an: %s' main

printf '\n\n# 3. Why her code is the way it is\n'
run kito why src/shop.py:discount | head -8 | cut -c1-100

printf '\n\n# 4. Grace pulls with git\n'
printf '\n(grace) $ git pull && cat src/shop.py | head -5\n'
grace pull -q origin main
head -5 "$GRACE/src/shop.py"

printf '\n\n# 5. A commit that conflicts with a lane waits for a decision\n'
sed -i 's|        result += round(price \* 100)|        result += int(price * 100)|' "$GRACE/src/shop.py"
grace commit -qam "cut prices, do not round them"
grace push -q origin main
printf '\n(grace pushes "cut prices, do not round them"; an agent changes the same line)\n'
LANE="$(kito lane open agent-b --intent "round half up" --scope src/shop.py:total | head -1 | cut -d' ' -f1)"
sed -i 's|        result += round(price \* 100)|        result += int(price * 100 + 0.5)|' "$SHOP/.kito/lanes/${LANE#lane/}/work/src/shop.py"
run kito integrate "$LANE"
kito lane close "$LANE" > /dev/null
for _ in $(seq 200); do kito status | grep -q 'wait to be merged' && break; pause 0.05; done
printf '\n$ kito status | grep -A1 "Git push"\n'
kito status | grep -A1 'Git push' | tidy | cut -c1-130
run kito conflicts
CONFLICT="$(kito conflicts | grep -o 'conflict#[0-9]*' | head -1)"
run kito resolve "$CONFLICT" --main | tidy
run kito pull | tidy
at_remote "$(git -C "$SHOP" rev-parse main)"
printf '\n$ git log --graph --oneline -n 4 main     # at the remote\n'
git -C "$REMOTE" log --graph --oneline --format='%h %an: %s' -n 4 main
printf '\n$ grep "result +=" src/shop.py     # what was decided\n'
grep 'result +=' src/shop.py
