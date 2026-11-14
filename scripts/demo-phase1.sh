#!/usr/bin/env bash
# Phase 1 demo: you work in a directory without committing, `kito log`
# shows everything, `git log` sees the checkpoints.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase1.sh
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
save() { printf '\n# saving %s\n' "$1"; mkdir -p "$(dirname "$1")"; cat > "$1"; sleep 0.2; }

run kito init
printf '.kito/\n' > .gitignore

save src/auth.ts <<'TS'
export function getUser(id: string) {
  return db.find(id);
}
TS
save README.md <<'MD'
# Demo project
MD
save src/auth.ts <<'TS'
export function getUser(id: string) {
  return cache.get(id) ?? db.find(id);
}
TS

# No commits so far: the log is already there.
run kito log
run kito checkpoint -m "auth with cache"

printf '\n# renaming README.md to GUIDE.md and adding a file\n'
mv README.md GUIDE.md
sleep 0.2
save src/billing.ts <<'TS'
import { getUser } from "./auth";
export const invoice = (id: string) => getUser(id);
TS
run kito checkpoint -m "billing"

run kito log
run git log --format='%h  %an  %s'
run git status --short
run git fsck --strict --no-dangling
run kito daemon status
