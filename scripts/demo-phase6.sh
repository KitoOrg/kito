#!/usr/bin/env bash
# Phase 6 demo: the intent board. Three agents on one machine.
#
# 1. No lane opens without saying why and what it will change.
# 2. The second agent declares the function the first one is changing: it
#    is told at once, with what the first is there for, before it writes.
# 3. The third declares a function that calls it: told too.
# 4. The board, and who is on a function.
# 5. A change outside the declared scope is recorded, and stops the lane
#    until the scope is extended.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase6.sh
set -euo pipefail

# Kito alone: `kito init` would also set the project up for Claude Code.
export KITO_NO_CLAUDE=1

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-francesco}"
DEMO="$(mktemp -d)"
trap 'kito --repo "$DEMO" daemon stop >/dev/null 2>&1 || true; rm -rf "$DEMO"' EXIT
cd "$DEMO"

run() { printf '\n$ %s\n' "$*"; "$@"; }
refused() { printf '\n$ %s\n' "$*"; "$@" || printf '(exit status %s)\n' "$?"; }

printf '.kito/\n__pycache__/\n' > .gitignore
cat > shop.py <<'PY'
def total(price, quantity):
    subtotal = price * quantity
    tax = subtotal * 0.22
    discount = 0
    shipping = 5
    return subtotal + tax + shipping - discount


def receipt(price, quantity):
    return "to pay: " + str(total(price, quantity))
PY
printf '# Shop\n\nPrices are in euros.\n' > README.md
run kito init
run kito checkpoint -m "the shop"

printf '\n\n# 1. No lane without an intent and a scope\n'
refused kito lane open hugo
refused kito lane open hugo --intent "look around" --scope ../elsewhere

printf '\n\n# 2. erin starts changing what a price is; frank declares the same function\n'
run kito lane open erin --intent "prices in cents, not euros" --scope shop.py:total
run kito lane open frank --intent "a discount of 10 over 50 euros" --scope shop.py:total

printf '\n\n# 3. gina declares a function that calls it\n'
run kito lane open gina --intent "a receipt with the currency" --scope shop.py:receipt

printf '\n\n# 4. The board, and who is on a function\n'
run kito board
run kito who shop.py:total

printf '\n\n# 5. erin does her work, and one thing she did not declare\n'
ERIN="$DEMO/.kito/lanes/1/work"
sed -i 's|subtotal = price \* quantity|subtotal = price * quantity / 100|' "$ERIN/shop.py"
sed -i 's|Prices are in euros.|Prices are in cents.|' "$ERIN/README.md"
refused kito integrate lane/1
run kito lane scope lane/1 --add README.md
run kito integrate lane/1
run kito log -n 3
