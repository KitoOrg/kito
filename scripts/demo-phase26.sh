#!/usr/bin/env bash
# Phase 26 demo: what agents are told is kept by Kito, not in the project.
#
# 1. A new project: the files Claude Code reads are written, and stay out
#    of the history and of git.
# 2. A kitchen first, the repository's own word over it.
# 3. What each assistant is told.
# 4. A CLAUDE.md that was in the project becomes an instruction.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase26.sh
set -euo pipefail

export KITO_AUTHOR=ada
unset KITO_NO_CLAUDE
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
DEMO="$(mktemp -d)"
export HOME="$DEMO/home" KITO_CONFIG_DIR="$DEMO/config" KITO_REGISTRY="file://$DEMO/no-registry"
mkdir -p "$HOME"
export PATH="$BIN:/usr/bin:/bin"
trap 'for d in "$DEMO"/shop "$DEMO"/books; do kito --repo "$d" daemon stop >/dev/null 2>&1 || true; done; rm -rf "$DEMO"' EXIT

tidy() { sed -e "s#$DEMO#/demo#g"; }
run() { printf '\n$ %s\n' "$*" | tidy; "$@" 2>&1 | tidy || true; }

echo "== 1. A new project"
mkdir -p "$DEMO/shop" && cd "$DEMO/shop"
echo 'export const total = (cents: number[]) => cents.reduce((a, b) => a + b, 0);' > shop.ts
run kito init --assistant claude
run kito sync
run kito status
run cat .gitignore
run sed -n '1,3p' CLAUDE.md

echo
echo "== 2. A kitchen first, the repository's own word over it"
mkdir -p "$DEMO/kitchen/instructions/all" "$DEMO/kitchen/instructions/codex"
printf '# Money\n\nAmounts are whole cents, never floats.\n' > "$DEMO/kitchen/instructions/all/money.md"
printf '# Style\n\nTwo spaces.\n' > "$DEMO/kitchen/instructions/all/style.md"
printf '# Plans\n\nWrite the plan before the first edit.\n' > "$DEMO/kitchen/instructions/codex/plans.md"
run kito instructions kitchen "$DEMO/kitchen"
printf '# Style\n\nTabs, as the code already has.\n' > "$DEMO/style.md"
run kito instructions set style --file "$DEMO/style.md"
run kito instructions

echo
echo "== 3. What each assistant is told"
run tail -n 12 CLAUDE.md
run sh -c "kito instructions show --for codex | tail -n 10"

echo
echo "== 4. A CLAUDE.md that was in the project"
mkdir -p "$DEMO/books" && cd "$DEMO/books"
printf '# Books\n\nTitles are kept as the publisher wrote them.\n' > CLAUDE.md
echo 'export const title = "Dune";' > books.ts
run kito init --no-assistants
run kito assistant add claude
run kito instructions
