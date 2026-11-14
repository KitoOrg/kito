#!/usr/bin/env bash
# Demo della fase 1: lavori in una cartella senza fare commit, `kito log`
# mostra tutto, `git log` vede i checkpoint.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-fase1.sh
set -euo pipefail

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-francesco}"
DEMO="$(mktemp -d)"
trap 'kito --repo "$DEMO" daemon stop >/dev/null 2>&1 || true; rm -rf "$DEMO"' EXIT
cd "$DEMO"

run() { printf '\n$ %s\n' "$*"; "$@"; }
save() { printf '\n# salvo %s\n' "$1"; mkdir -p "$(dirname "$1")"; cat > "$1"; sleep 0.2; }

run kito init
printf '.kito/\n' > .gitignore

save src/auth.ts <<'TS'
export function getUser(id: string) {
  return db.find(id);
}
TS
save README.md <<'MD'
# Progetto demo
MD
save src/auth.ts <<'TS'
export function getUser(id: string) {
  return cache.get(id) ?? db.find(id);
}
TS

# Nessun commit finora: il log c'è già.
run kito log
run kito checkpoint -m "auth con cache"

printf '\n# rinomino README.md in LEGGIMI.md e aggiungo un file\n'
mv README.md LEGGIMI.md
sleep 0.2
save src/billing.ts <<'TS'
import { getUser } from "./auth";
export const invoice = (id: string) => getUser(id);
TS
run kito checkpoint -m "fatturazione"

run kito log
run git log --format='%h  %an  %s'
run git status --short
run git fsck --strict --no-dangling
run kito daemon status
