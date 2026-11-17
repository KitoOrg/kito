#!/usr/bin/env bash
# Phase 2 demo: a function is renamed, then moved to another file, then
# edited there. `kito why` tells its whole story from the name it has today;
# git, for the same saves, sees lines removed in one file and added in another.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase2.sh
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
save() { printf '\n# saving %s\n' "$1"; mkdir -p "$(dirname "$1")"; cat > "$1"; sleep 0.2; }

run kito init
printf '.kito/\n' > .gitignore

save src/auth.ts <<'TS'
import { db } from "./db";

export function getUser(id: string): User {
  const cached = cache.get(id);
  if (cached) {
    return cached;
  }
  const user = db.find(id);
  cache.set(id, user);
  return user;
}

export class Session {
  constructor(private user: User) {}

  isValid(now: number): boolean {
    return this.user.expires > now && !this.user.revoked;
  }

  refresh(now: number): void {
    this.user.expires = now + SESSION_LENGTH;
    audit.log("refresh", this.user.id);
  }
}
TS
run kito checkpoint -m "auth"

printf '\n# 1. the function is renamed: getUser -> fetchUser\n'
sed -i 's/getUser/fetchUser/' src/auth.ts
sleep 0.2
run kito log -n 1

printf '\n# 2. the function is cut from src/auth.ts and pasted into a new file, src/users.ts\n'
mkdir -p src
sed -n '/^export function fetchUser/,/^}/p' src/auth.ts > src/users.ts
sed -i '/^export function fetchUser/,/^}/d' src/auth.ts
sleep 0.2
run kito log -n 2

printf '\n# 3. its body changes in the new file\n'
sed -i 's/cache.set(id, user);/cache.set(id, user, TTL);/' src/users.ts
sleep 0.2

printf '\n# 4. a method leaves its class for another one: refresh goes from Session to a new class\n'
python3 - <<'PY'
import re
text = open("src/auth.ts").read()
method = re.search(r"\n  refresh\(now: number\): void \{.*?\n  \}\n", text, re.S).group(0)
text = text.replace(method, "") + "\nexport class Renewal {" + method + "}\n"
open("src/auth.ts", "w").write(text)
PY
sleep 0.2

# One symbol, one ID, the whole story: asked by the name and the place it has now.
run kito why --history src/users.ts:fetchUser
run kito why --history src/auth.ts:Renewal.refresh
# The method nobody touched has a one-line story.
run kito why --history src/auth.ts:Session.isValid

run kito log
# The log of one file: its changes, and the symbols that were in it at the time.
run kito log src/users.ts
run kito checkpoint -m "users split from auth"

# What git can say about the same work: lines went away here and appeared there.
run git log --stat --format='%h  %an  %s'
run git log --follow --format='%h  %s' -- src/users.ts
run kito status
