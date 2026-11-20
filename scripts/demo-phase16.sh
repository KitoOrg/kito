#!/usr/bin/env bash
# Phase 16 demo: permanent lanes.
#
# 1. A `staging` lane next to `main`, with checks of its own.
# 2. Two agents land on staging: checkpoints of staging, its own git
#    branch, pushed with main's; main does not move.
# 3. A fix lands on main directly; staging catches up with it.
# 4. Staging lands on main as a git merge, and stays.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase16.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-ada}"
DEMO="$(mktemp -d)"
APP="$DEMO/app"
REMOTE="$DEMO/remote.git"
stop() {
  kito --repo "$APP" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT

run() { printf '\n$ %s\n' "$*"; "$@"; }
pause() { read -r -t "$1" _ < /dev/zero || true; }

mkdir -p "$APP/src"; cd "$APP"
cat > src/api.py <<'PY'
def get_user(user_id):
    return {"id": user_id}
PY
cat > src/page.py <<'PY'
def render(user_id):
    return "user " + str(user_id)
PY
printf '.kito/\n' > .gitignore
git init -q --bare -b main "$REMOTE"
kito init . > /dev/null
sed -i "s|^# push = \"origin\"|push = \"$REMOTE\"|" .kito/config.toml
cat >> .kito/config.toml <<'TOML'

[lanes.staging]
verify = ["python3 -c 'import sys; sys.path.insert(0, \"src\"); import api, page'"]
TOML
kito daemon stop > /dev/null
kito checkpoint -m "an app with an API and a page" > /dev/null

printf '# 1. A permanent lane next to main\n'
run kito lane create staging
run kito lanes

printf '\n\n# 2. Work lands on staging, under checkpoints of its own\n'
kito lane open alice --intent "users have a team" --scope src/api.py --on staging > /dev/null
cat > .kito/lanes/2/work/src/api.py <<'PY'
def get_user(user_id):
    return {"id": user_id, "team": "core"}
PY
run kito integrate 2
kito lane open bob --intent "show the team" --scope src/page.py --on staging > /dev/null
cat > .kito/lanes/3/work/src/page.py <<'PY'
from api import get_user


def render(user_id):
    user = get_user(user_id)
    return "user " + str(user["id"]) + " of " + user["team"]
PY
run kito integrate 3
for _ in $(seq 100); do git -C "$REMOTE" rev-parse -q --verify staging > /dev/null && break; pause 0.05; done
printf '\n$ git log --graph --oneline --all     # here; the remote has both branches too\n'
git log --graph --format='%h %d %s' --all
printf '\n$ git -C remote.git branch\n'; git -C "$REMOTE" branch

printf '\n\n# 3. A fix lands on main directly; staging catches up\n'
kito lane open carol --intent "a readme" --scope README.md > /dev/null
printf '# App\n' > .kito/lanes/4/work/README.md
run kito integrate 4
run kito lane catch-up staging

printf '\n\n# 4. Staging lands on main as a merge, and stays\n'
run kito integrate staging
printf '\n$ git log --graph --oneline --all\n'
git log --graph --format='%h %d %s' --all
run kito lanes
run cat src/page.py
