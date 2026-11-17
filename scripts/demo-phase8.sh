#!/usr/bin/env bash
# Phase 8 demo: lanes from any machine.
#
# 1. A server with one repository and two people.
# 2. Ada connects a directory (her laptop): the main lane's files are in
#    it. She opens a lane for her agent: the lane is on the server, its
#    working copy is on her machine.
# 3. Bob connects another (his desk) and follows her lane.
# 4. Twenty saves on ada's machine, each timed until it is on bob's disk.
# 5. Ada's daemon is killed in the middle of the work. The saves go on;
#    the next command starts it again, and the lane has every one of them.
# 6. The server goes away: saves wait on ada's machine, and go when it is
#    back.
# 7. The lane is integrated from ada's machine; bob's directory, which
#    follows the main lane, has it.
#
#   cargo build --release -p kitod -p kito-cli -p kito-server
#   scripts/demo-phase8.sh
set -euo pipefail

# Kito alone: `kito connect` would also set the directory up for Claude Code.
export KITO_NO_CLAUDE=1

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-the server}"
DEMO="$(mktemp -d)"
SERVER="$DEMO/server"
stop() {
  for dir in laptop desk; do kito --repo "$DEMO/$dir" daemon stop >/dev/null 2>&1 || true; done
  [ -n "${SERVING:-}" ] && kill "$SERVING" 2>/dev/null || true
  kito --repo "$SERVER/repos/shop" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT
cd "$DEMO"

run() { printf '\n$ %s\n' "$*"; "$@"; }
refused() { printf '\n$ %s\n' "$*"; "$@" || printf '(exit status %s)\n' "$?"; }
us() { date +%s%6N; }
pause() { read -r -t "$1" _ < /dev/zero || true; }
# The same files on both sides, apart from what Kito keeps for itself.
same() { diff -r -x .kito "$1" "$2" >/dev/null 2>&1; }
until_same() { local n=0; until same "$1" "$2"; do n=$((n + 1)); [ "$n" -lt 5000 ] || return 1; pause 0.002; done; }

mkdir -p "$DEMO/shop-src/src"
printf 'def total(price, quantity):\n    return price * quantity\n\n\ndef discount(amount, percent):\n    return amount - amount * percent / 100\n' > "$DEMO/shop-src/src/shop.py"
printf '# Shop\n' > "$DEMO/shop-src/README.md"
git -C "$DEMO/shop-src" init -q -b main && git -C "$DEMO/shop-src" add -A
git -C "$DEMO/shop-src" -c user.name=demo -c user.email=demo@example.com commit -q -m start

serve() {
  kito-server --dir "$SERVER" serve --listen "$1" > "$DEMO/serving" 2>/dev/null &
  SERVING=$!
  until [ -s "$DEMO/serving" ]; do pause 0.05; done
  ADDR="$(sed -n 's/^kito-server: listening on \([^,]*\),.*/\1/p' "$DEMO/serving")"
}

printf '# 1. The server: one repository, two people\n'
run kito-server init "$SERVER"
run kito-server --dir "$SERVER" repo add shop --from "$DEMO/shop-src"
ADA="$(kito-server --dir "$SERVER" user add ada | grep '^kito_')"
BOB="$(kito-server --dir "$SERVER" user add bob | grep '^kito_')"
printf '\n$ kito-server --dir %s user add ada; … user add bob\n(two tokens, each shown once)\n' "$SERVER"
printf '\n$ kito-server --dir %s serve --listen 127.0.0.1:0 &\n' "$SERVER"
serve 127.0.0.1:0
cat "$DEMO/serving"

printf '\n\n# 2. Ada, on her laptop: a lane on the server, its files on her machine\n'
printf '\n$ kito connect %s shop laptop --token kito_…\n' "$ADDR"
kito connect "$ADDR" shop laptop --token "$ADA"
cd "$DEMO/laptop"
run ls . src
run kito lane open agent-a --intent "prices in cents" --scope src/ README.md
WORK="$DEMO/laptop/.kito/lanes/1/work"
THERE="$SERVER/repos/shop/.kito/lanes/1/work"
run ls "$WORK" "$WORK/src"

printf '\n\n# 3. Bob, at his desk: he follows her lane\n'
cd "$DEMO"
printf '\n$ kito connect %s shop desk --token kito_…\n' "$ADDR"
kito connect "$ADDR" shop desk --token "$BOB"
cd "$DEMO/desk"
run kito lane attach 1
VIEW="$DEMO/desk/.kito/lanes/1/work"

printf '\n\n# 4. Twenty saves on the laptop, each timed until it is on the desk'"'"'s disk\n'
TIMES=()
for n in $(seq 1 20); do
  START="$(us)"
  printf 'def total(price, quantity):\n    return round(price * 100) * quantity + %s\n\n\ndef discount(amount, percent):\n    return amount - amount * percent / 100\n' "$n" > "$WORK/src/shop.py"
  until cmp -s "$WORK/src/shop.py" "$VIEW/src/shop.py"; do :; done
  TIMES+=($(( ($(us) - START) / 1000 )))
done
SORTED=($(printf '%s\n' "${TIMES[@]}" | sort -n))
printf '\nmilliseconds from the save to the follower'"'"'s disk, 20 saves: median %s, slowest %s\n' "${SORTED[9]}" "${SORTED[19]}"
(cd "$DEMO/desk" && run kito log --lane 1 -n 3)

printf '\n\n# 5. Ada'"'"'s daemon is killed while the agent works\n'
cd "$DEMO/laptop"
PID="$(kito --json daemon status | sed -n 's/.*"pid": *\([0-9]*\).*/\1/p')"
printf '\n$ kill -9 %s    # the daemon of the laptop\n' "$PID"
kill -9 "$PID"
printf 'TAX = 22\n' > "$WORK/src/tax.py"
printf '# Shop\n\nPrices are in cents.\n' > "$WORK/README.md"
printf '(two more saves, with no daemon to see them)\n'
run kito sync 1
if same "$WORK" "$THERE"; then
  printf 'The lane on the server has exactly what is on the laptop.\n'
else
  printf 'DIFFERENT: the lane on the server and the laptop\n'; exit 1
fi
run kito daemon status

printf '\n\n# 6. The server goes away; the saves wait, then go\n'
printf '\n$ kill %s    # the server\n' "$SERVING"
kill "$SERVING"; wait "$SERVING" 2>/dev/null || true
printf 'VAT = 22\n' > "$WORK/src/tax.py"
refused kito sync 1
run kito daemon status
printf '\n$ kito-server --dir %s serve --listen %s &\n' "$SERVER" "$ADDR"
: > "$DEMO/serving"
serve "$ADDR"
run kito sync 1
same "$WORK" "$THERE" && printf 'The lane on the server has exactly what is on the laptop.\n'
until_same "$WORK" "$VIEW" && printf 'So has bob, at his desk.\n'

printf '\n\n# 7. Integrated from the laptop; bob'"'"'s directory has it\n'
MAIN="$DEMO/desk"
run kito integrate 1
until cmp -s "$WORK/src/shop.py" "$MAIN/src/shop.py"; do pause 0.01; done
run cat "$MAIN/src/tax.py"
(cd "$DEMO/desk" && run kito daemon status)
