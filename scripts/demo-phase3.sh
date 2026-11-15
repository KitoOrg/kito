#!/usr/bin/env bash
# Phase 3 demo, in two parts.
#
# 1. Two agents, by hand: each works in a lane of its own, on the same file
#    and at the same place in it. Their work is integrated without a
#    conflict where git would report one; a real conflict (the same line
#    changed in two ways) is reported as such, decided, and integrated.
# 2. A swarm: ten simulated agents, four rounds each, on a TypeScript
#    project, every integration verified by the type checker.
#
#   cargo build --release -p kitod -p kito-cli
#   cargo build --release -p kitod --example swarm
#   scripts/demo-phase3.sh
set -euo pipefail

# Kito alone: `kito init` would also set the project up for Claude Code.
export KITO_NO_CLAUDE=1

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-francesco}"
export KITO_EMAIL="${KITO_EMAIL:-francesco@example.com}"
DEMO="$(mktemp -d)"
trap 'kito --repo "$DEMO" daemon stop >/dev/null 2>&1 || true; rm -rf "$DEMO"' EXIT
cd "$DEMO"

run() { printf '\n$ %s\n' "$*"; "$@"; }
# What `run` does, for commands that are expected to say no.
refused() { printf '\n$ %s\n' "$*"; "$@" || printf '(exit status %s)\n' "$?"; }

printf '# Part 1: two agents, one file\n'
run kito init
printf '.kito/\n' > .gitignore
mkdir -p src
cat > src/shop.ts <<'TS'
// Prices are in cents.
export function total(items: Item[]): number {
  let sum = 0;
  for (const item of items) {
    sum += item.price * item.quantity;
  }
  return sum;
}

export function discount(amount: number, percent: number): number {
  const cut = Math.round((amount * percent) / 100);
  return amount - cut;
}
TS
run kito checkpoint -m "shop"

run kito lane open alice --intent "taxes" --scope src/shop.ts
run kito lane open bob --intent "shipping" --scope src/shop.ts
ALICE="$DEMO/.kito/lanes/1/work"
BOB="$DEMO/.kito/lanes/2/work"

printf '\n# alice, in her working copy: a new function at the end of the file, and a fix in `total`\n'
sed -i 's/let sum = 0;/let sum = 0; \/\/ cents/' "$ALICE/src/shop.ts"
cat >> "$ALICE/src/shop.ts" <<'TS'

export function tax(amount: number): number {
  return Math.round(amount * 0.22);
}
TS
printf '# bob, in his: another new function at the same place, and a fix on another line of `total`\n'
sed -i 's/  return sum;/  return Math.round(sum);/' "$BOB/src/shop.ts"
cat >> "$BOB/src/shop.ts" <<'TS'

export function shipping(weight: number): number {
  return weight > 1000 ? 900 : 500;
}
TS
run kito sync
run kito lanes

printf '\n# git, on the same two versions of the file: two functions added at the same place\n'
printf '\n$ git merge-file -p alice/src/shop.ts src/shop.ts bob/src/shop.ts | tail -n 8\n'
CONFLICTS=0
MERGED="$(mktemp)"
git merge-file -p -L alice -L base -L bob "$ALICE/src/shop.ts" src/shop.ts "$BOB/src/shop.ts" > "$MERGED" \
  || CONFLICTS="$?"
tail -n 8 "$MERGED"
rm -f "$MERGED"
printf '(git merge-file: %s conflict)\n' "$CONFLICTS"

run kito integrate lane/1 lane/2
run cat src/shop.ts
run kito log -n 6

printf '\n# Now a real conflict: the same line of `discount`, changed in two ways\n'
sed -i 's/Math.round((amount \* percent) \/ 100)/Math.floor((amount * percent) \/ 100)/' "$ALICE/src/shop.ts"
sed -i 's/Math.round((amount \* percent) \/ 100)/Math.ceil((amount * percent) \/ 100)/' "$BOB/src/shop.ts"
run kito sync
run kito integrate lane/1
refused kito integrate lane/2
run kito conflicts
CONFLICT="$(kito conflicts --json | sed -n 's/.*"id": *"\(conflict#[0-9]*\)".*/\1/p' | head -n 1)"
run kito conflicts "$CONFLICT"
run kito resolve "$CONFLICT" --lane
run kito integrate lane/2
run grep -n "percent) / 100" src/shop.ts
run kito why src/shop.ts:discount
run kito lane close lane/1
run kito lane close lane/2
run kito export-git
run git log --oneline
run git fsck --strict --no-dangling
kito daemon stop >/dev/null 2>&1 || true

printf '\n\n# Part 2: a swarm of ten agents\n'
run "$BIN/examples/swarm" --agents 10 --rounds 4
