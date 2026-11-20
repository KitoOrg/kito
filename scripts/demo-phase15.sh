#!/usr/bin/env bash
# Phase 15 demo: lanes on lanes.
#
# 1. Alice's agent changes the API in a lane. Before it lands, Bob's agent
#    opens a lane on top of hers and builds the page on the new API.
# 2. Bob's lane lands on Alice's: no checkpoint, the main lane does not
#    move, and Alice's lane now carries both.
# 3. Carol's agent writes tests on top of Alice's lane but lands on main:
#    it waits until Alice's work is there, then brings only its own.
# 4. Alice's lane cannot be closed while work built on it has not landed.
# 5. Why the API is the way it is, asked from Bob's lane: Alice's reason.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase15.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-ada}"
DEMO="$(mktemp -d)"
APP="$DEMO/app"
stop() {
  kito --repo "$APP" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT

run() { printf '\n$ %s\n' "$*"; "$@"; }
tidy() { sed "s|$DEMO|/demo|g"; }

mkdir -p "$APP/src"; cd "$APP"
cat > src/api.py <<'PY'
def get_user(user_id):
    return {"id": user_id}
PY
cat > src/page.py <<'PY'
from api import get_user


def render(user_id):
    return "user " + str(get_user(user_id)["id"])
PY
printf '.kito/\n' > .gitignore
kito init . > /dev/null
kito checkpoint -m "an app with an API and a page" > /dev/null

printf '# 1. A lane on a lane that has not landed\n'
run kito lane open alice --intent "users have a team" --scope src/api.py
cat >> .kito/lanes/1/work/src/api.py <<'PY'


def get_team(user_id):
    return {"team": "core", "user": user_id}
PY
run kito lane open bob --intent "show the team on the page" --scope src/page.py --on 1
printf '\n$ grep -c get_team .kito/lanes/2/work/src/api.py     # bob sees alice'"'"'s work\n'
grep -c get_team .kito/lanes/2/work/src/api.py
cat > .kito/lanes/2/work/src/page.py <<'PY'
from api import get_team, get_user


def render(user_id):
    team = get_team(user_id)["team"]
    return "user " + str(get_user(user_id)["id"]) + " of " + team
PY

printf '\n\n# 2. It lands on the lane it started on\n'
run kito integrate 2
printf '\n$ grep -c team .kito/lanes/1/work/src/page.py src/page.py\n'
grep -c team .kito/lanes/1/work/src/page.py src/page.py || true
run kito log -n 2

printf '\n\n# 3. On alice'"'"'s lane, landing on main: it waits for her\n'
run kito lane open carol --intent "the team is tested" --scope tests/ --on 1 --target main
mkdir -p .kito/lanes/3/work/tests
cat > .kito/lanes/3/work/tests/test_team.py <<'PY'
from api import get_team


def test_team():
    assert get_team(7)["team"] == "core"
PY
run kito lanes
run kito integrate 3 || true
run kito integrate 1
run kito integrate 3

printf '\n\n# 4. A lane under others closes once their base has landed\n'
run kito lane close 1
run kito lanes

printf '\n\n# 5. Why the API is the way it is, from bob'"'"'s lane\n'
run kito why src/api.py:get_team --lane 2 | head -6 | cut -c1-100
run kito log -n 4
