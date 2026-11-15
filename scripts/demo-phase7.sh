#!/usr/bin/env bash
# Phase 7 demo: one server, two repositories, two people.
#
# 1. A server with two repositories and two people: ada may use both, bob
#    only the shop.
# 2. Each connects a directory of their own to the shop.
# 3. Bob watches. Ada opens a lane: bob sees it, and the time it took.
# 4. Bob's board says whose lane it is; his own lane on the same function
#    is told of hers.
# 5. What is refused: a wrong token, a repository that is not bob's, and
#    what belongs to whoever runs the server.
# 6. The other repository knows nothing of the first.
#
#   cargo build --release -p kitod -p kito-cli -p kito-server
#   scripts/demo-phase7.sh
set -euo pipefail

# Kito alone: `kito init` would also set the project up for Claude Code.
export KITO_NO_CLAUDE=1

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-the server}"
DEMO="$(mktemp -d)"
SERVER="$DEMO/server"
stop() {
  [ -n "${SERVING:-}" ] && kill "$SERVING" 2>/dev/null || true
  for repo in shop site; do kito --repo "$SERVER/repos/$repo" daemon stop >/dev/null 2>&1 || true; done
  rm -rf "$DEMO"
}
trap stop EXIT
cd "$DEMO"

run() { printf '\n$ %s\n' "$*"; "$@"; }
refused() { printf '\n$ %s\n' "$*"; "$@" || printf '(exit status %s)\n' "$?"; }
ms() { date +%s%3N; }

# Two git repositories for the server to host.
source_repo() {
  mkdir -p "$1/src" && printf '%s' "$3" > "$1/$2"
  git -C "$1" init -q -b main && git -C "$1" add -A
  git -C "$1" -c user.name=demo -c user.email=demo@example.com commit -q -m start
}
source_repo "$DEMO/shop-src" src/shop.py 'def total(price, quantity):
    return price * quantity
'
source_repo "$DEMO/site-src" src/index.py 'TITLE = "the site"
'

printf '# 1. The server: two repositories, two people\n'
run kito-server init "$SERVER"
run kito-server --dir "$SERVER" repo add shop --from "$DEMO/shop-src"
run kito-server --dir "$SERVER" repo add site --from "$DEMO/site-src"
printf '\n$ kito-server --dir %s user add ada --email ada@example.com\n' "$SERVER"
ADA="$(kito-server --dir "$SERVER" user add ada --email ada@example.com | grep '^kito_')"
printf 'ada added. Their token, shown this once:\nkito_…\n'
printf '\n$ kito-server --dir %s user add bob --repo shop\n' "$SERVER"
BOB="$(kito-server --dir "$SERVER" user add bob --repo shop | grep '^kito_')"
printf 'bob added. Their token, shown this once:\nkito_…\n'
run kito-server --dir "$SERVER" user list

printf '\n$ kito-server --dir %s serve --listen 127.0.0.1:0 &\n' "$SERVER"
kito-server --dir "$SERVER" serve --listen 127.0.0.1:0 > "$DEMO/serving" 2>/dev/null &
SERVING=$!
until [ -s "$DEMO/serving" ]; do read -r -t 0.05 _ < /dev/zero || true; done
cat "$DEMO/serving"
ADDR="$(sed -n 's/^kito-server: listening on \([^,]*\),.*/\1/p' "$DEMO/serving")"

printf '\n\n# 2. Each connects a directory of their own\n'
printf '\n$ kito connect %s shop ada-shop --token kito_…\n' "$ADDR"
kito connect "$ADDR" shop ada-shop --token "$ADA"
printf '\n$ kito connect %s shop bob-shop --token kito_…\n' "$ADDR"
kito connect "$ADDR" shop bob-shop --token "$BOB"
(cd ada-shop && run kito repos)
(cd bob-shop && run kito repos)

printf '\n\n# 3. Bob watches; ada opens a lane\n'
printf '\n$ (bob) kito watch -n 1 &\n'
(cd bob-shop && kito watch -n 1 > "$DEMO/heard" && ms > "$DEMO/heard-at") &
WATCHING=$!
read -r -t 0.3 _ < /dev/zero || true
ASKED="$(ms)"
(cd ada-shop && run kito lane open builder --intent "prices in cents" --scope src/shop.py:total)
wait "$WATCHING"
printf '\n(bob) '; cat "$DEMO/heard"
printf 'Bob saw it %s ms after ada asked.\n' "$(( $(cat "$DEMO/heard-at") - ASKED ))"

printf '\n\n# 4. Whose lane it is, and a second lane on the same function\n'
(cd bob-shop && run kito board)
(cd bob-shop && run kito lane open fixer --intent "a discount over 50" --scope src/shop.py:total)

printf '\n\n# 5. What is refused\n'
printf '\n$ kito connect %s shop nope --token kito_wrong\n' "$ADDR"
kito connect "$ADDR" shop nope --token kito_wrong || printf '(exit status %s)\n' "$?"
printf '\n$ (bob) kito connect %s site bob-site --token kito_…\n' "$ADDR"
kito connect "$ADDR" site bob-site --token "$BOB" || printf '(exit status %s)\n' "$?"
(cd bob-shop && refused kito checkpoint -m "from afar")
(cd bob-shop && refused kito daemon stop)

printf '\n\n# 6. The other repository knows nothing of the first\n'
printf '\n$ kito connect %s site ada-site --token kito_…\n' "$ADDR"
kito connect "$ADDR" site ada-site --token "$ADA"
(cd ada-site && run kito lanes)
(cd ada-shop && run kito lanes)
