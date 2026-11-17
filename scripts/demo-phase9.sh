#!/usr/bin/env bash
# Phase 9 demo: a protected main lane and the rules of a team.
#
# 1. A server with one repository, its test as the check every lane must
#    pass, a git remote that gets every integration, and three people:
#    ada may integrate, bob may write, carol may read.
# 2. Somebody edits the main directory on the server: the file is put
#    back, the edit is kept aside, and whoever watches is told.
# 3. Bob's agent breaks the test: the lane is stopped, and nothing reaches
#    the main lane.
# 4. Fixed, the lane passes and waits: bringing it in is not for bob.
# 5. Ada brings it in. The commit is on the git remote.
# 6. Carol sees and follows, and changes nothing; bob does not decide for
#    a lane that is ada's.
#
# Needs python3 (the test of the demo repository) and git.
#
#   cargo build --release -p kitod -p kito-cli -p kito-server
#   scripts/demo-phase9.sh
set -euo pipefail

# Kito alone: `kito connect` would also set the directory up for Claude Code.
export KITO_NO_CLAUDE=1

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-the server}"
DEMO="$(mktemp -d)"
SERVER="$DEMO/server"
stop() {
  for dir in ada bob carol; do kito --repo "$DEMO/$dir" daemon stop >/dev/null 2>&1 || true; done
  [ -n "${SERVING:-}" ] && kill "$SERVING" 2>/dev/null || true
  kito --repo "$SERVER/repos/shop" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT
cd "$DEMO"

run() { printf '\n$ %s\n' "$*"; "$@"; }
refused() { printf '\n$ %s\n' "$*"; "$@" || printf '(exit status %s)\n' "$?"; }
pause() { read -r -t "$1" _ < /dev/zero || true; }
as() { local who="$1"; shift; (cd "$DEMO/$who" && printf '\n(%s) $ %s\n' "$who" "$*" && "$@"); }
as_refused() { local who="$1"; shift; (cd "$DEMO/$who" && printf '\n(%s) $ %s\n' "$who" "$*" && { "$@" || printf '(exit status %s)\n' "$?"; }); }

# The repository the server will host: a function and its test.
mkdir -p "$DEMO/shop-src/src" "$DEMO/shop-src/tests"
cat > "$DEMO/shop-src/src/shop.py" <<'PY'
def total(price, quantity):
    return price * quantity
PY
cat > "$DEMO/shop-src/tests/test_shop.py" <<'PY'
import sys
sys.path.insert(0, "src")
from shop import total

assert total(2, 3) == 6, "total(2, 3) should be 6, it is %r" % total(2, 3)
print("ok")
PY
git -C "$DEMO/shop-src" init -q -b main && git -C "$DEMO/shop-src" add -A
git -C "$DEMO/shop-src" -c user.name=demo -c user.email=demo@example.com commit -q -m start
git init -q --bare -b main "$DEMO/upstream.git"

printf '# 1. The server: a repository, its check, its git remote, three people\n'
run kito-server init "$SERVER"
run kito-server --dir "$SERVER" repo add shop --from "$DEMO/shop-src"
run kito-server --dir "$SERVER" repo set shop --check "python3 tests/test_shop.py" --push "$DEMO/upstream.git"
ADA="$(kito-server --dir "$SERVER" user add ada | grep '^kito_')"
BOB="$(kito-server --dir "$SERVER" user add bob --role write | grep '^kito_')"
CAROL="$(kito-server --dir "$SERVER" user add carol --role read | grep '^kito_')"
printf '\n$ kito-server --dir %s user add ada; … user add bob --role write; … user add carol --role read\n(three tokens, each shown once)\n' "$SERVER"
run kito-server --dir "$SERVER" user list
kito-server --dir "$SERVER" serve --listen 127.0.0.1:0 > "$DEMO/serving" 2>/dev/null &
SERVING=$!
until [ -s "$DEMO/serving" ]; do pause 0.05; done
ADDR="$(sed -n 's/^kito-server: listening on \([^,]*\),.*/\1/p' "$DEMO/serving")"
kito connect "$ADDR" shop ada --token "$ADA" > /dev/null
kito connect "$ADDR" shop bob --token "$BOB" > /dev/null
kito connect "$ADDR" shop carol --token "$CAROL" > /dev/null
printf '\n(each connects a directory of their own: ada, bob, carol)\n'

printf '\n\n# 2. A direct edit to the main lane is refused\n'
MAIN="$SERVER/repos/shop"
printf '\n$ echo "def total(price, quantity): return 0" > %s/src/shop.py    # on the server, by hand\n' "$MAIN"
echo "def total(price, quantity): return 0" > "$MAIN/src/shop.py"
until grep -q 'return price \* quantity' "$MAIN/src/shop.py"; do pause 0.02; done
run cat "$MAIN/src/shop.py"
(cd "$MAIN" && run kito status) | grep -E '^\$|Main lane|put back' | sed 's/^ */  /; s/^  \$/\n$/'
KEPT="$(ls -d "$MAIN"/.kito/refused/*/)"
run cat "${KEPT}src/shop.py"
printf '\n(carol) $ kito watch --past\n'
(cd "$DEMO/carol" && { timeout 1 kito watch --past || true; } | grep 'main edit refused')

printf '\n\n# 3. A failing test stops a lane\n'
as bob kito lane open agent-b --intent "prices in cents" --scope src/shop.py:total
WORK="$DEMO/bob/.kito/lanes/1/work"
printf '\n(the agent edits its working copy, on bob'"'"'s machine)\n'
cat > "$WORK/src/shop.py" <<'PY'
def total(price, quantity):
    return round(price * 100) * quantity
PY
as_refused bob kito integrate 1
run cat "$MAIN/src/shop.py"

printf '\n\n# 4. Fixed, it passes, and waits for somebody who may integrate\n'
printf '\n(the test is part of the change: the agent declares it, and updates it)\n'
as bob kito lane scope 1 --add tests/test_shop.py
cat > "$WORK/tests/test_shop.py" <<'PY'
import sys
sys.path.insert(0, "src")
from shop import total

assert total(2, 3) == 600, "total(2, 3) should be 600 cents, it is %r" % total(2, 3)
print("ok")
PY
as bob kito integrate 1
as ada kito board
run cat "$MAIN/src/shop.py"

printf '\n\n# 5. Ada brings it in; the commit is on the git remote\n'
as ada kito integrate 1
run cat "$MAIN/src/shop.py"
until [ "$(git -C "$DEMO/upstream.git" rev-list --count main 2>/dev/null)" = 2 ]; do pause 0.05; done
run git -C "$DEMO/upstream.git" log --format='%h %an: %s' main
(cd "$MAIN" && kito status) | grep -E 'Checks|Git push'

printf '\n\n# 6. Who may do what\n'
as_refused carol kito lane open agent-c --intent "a look around" --scope src/
as carol kito lane attach 1
as ada kito lane open agent-a --intent "a receipt" --scope src/receipt.py
as_refused bob kito lane close 2
