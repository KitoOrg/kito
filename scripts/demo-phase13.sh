#!/usr/bin/env bash
# Phase 13 demo: integration without waiting in line.
#
# The checks of this repository take two seconds. Five agents finish at
# the same time.
#
# 1. One lane at a time, as it was: five runs of the checks.
# 2. Together: one run for the five, five checkpoints.
# 3. One of four lanes breaks the checks: it is found, the others go in.
# 4. Two lanes on the same file do not wait for luck: they are merged
#    function by function, and checked one after the other.
#
# Needs python3 (the check of the demo repository).
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase13.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
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
# A command, and how long it took.
timed() {
  printf '\n$ %s\n' "$*"
  local start; start=$(date +%s%N)
  "$@" || true
  printf '(%s.%s s in all)\n' $(( ($(date +%s%N) - start) / 1000000000 )) $(( ($(date +%s%N) - start) / 100000000 % 10 ))
}
lane_dir() { printf '%s/.kito/lanes/%x/work' "$SHOP" "$1"; }

for name in cart prices taxes stock mail; do
  printf 'def %s():\n    return 1\n' "$name" > "src/$name.py"
done
printf 'def total(prices):\n    return sum(prices)\n\n\ndef discount(amount):\n    return amount\n' > src/shop.py
kito init . > /dev/null
sed -i 's|# commands = \["npx tsc --noEmit", "npm test"\]|commands = ["sleep 2 \&\& python3 -m py_compile src/*.py"]|' .kito/config.toml
kito daemon stop > /dev/null
kito checkpoint -m "a shop" > /dev/null
printf '# The checks of this repository take two seconds\n'
run grep -A1 '^\[verify\]' .kito/config.toml | grep -v '^#' || true
grep '^commands' .kito/config.toml

N=0
change() { # the files, one lane each: opens the lanes and changes each file
  LANES=()
  for name in "$@"; do
    N=$((N + 1))
    kito lane open "agent-$name" --intent "work on $name" --scope "src/$name.py" > /dev/null
    printf '    # changed by lane %s\n' "$N" >> "$(lane_dir $N)/src/$name.py"
    LANES+=("lane/$(printf '%x' $N)")
  done
}

printf '\n\n# 1. One lane at a time, as it was\n'
sed -i 's|^batch = 8|batch = 1|' .kito/config.toml; kito daemon stop > /dev/null
change cart prices taxes stock mail
timed kito integrate "${LANES[@]}"
for lane in "${LANES[@]}"; do kito lane close "$lane" > /dev/null; done

printf '\n\n# 2. Together: the lanes that do not depend on each other, in one run\n'
sed -i 's|^batch = 1|batch = 8|' .kito/config.toml; kito daemon stop > /dev/null
change cart prices taxes stock mail
timed kito integrate "${LANES[@]}"
run kito log -n 5
for lane in "${LANES[@]}"; do kito lane close "$lane" > /dev/null; done

printf '\n\n# 3. One of four breaks the checks\n'
change cart prices taxes stock
printf 'def broken(:\n' >> "$(lane_dir $((N - 1)))/src/taxes.py"
printf '\n(the lane on taxes.py now holds a line that is not Python)\n'
timed kito integrate "${LANES[@]}" 2>&1 | grep -v '^  |   ' | cut -c1-110
for lane in "${LANES[@]}"; do kito lane close "$lane" > /dev/null; done

printf '\n\n# 4. Two lanes on two functions of one file\n'
kito lane open agent-total --intent "totals in cents" --scope src/shop.py:total > /dev/null; N=$((N + 1)); A=$N
kito lane open agent-discount --intent "ten off" --scope src/shop.py:discount > /dev/null; N=$((N + 1)); B=$N
sed -i 's/    return sum(prices)/    return round(sum(prices) * 100)/' "$(lane_dir $A)/src/shop.py"
sed -i 's/    return amount$/    return amount - 10/' "$(lane_dir $B)/src/shop.py"
timed kito integrate "lane/$(printf '%x' $A)" "lane/$(printf '%x' $B)"
run cat src/shop.py
