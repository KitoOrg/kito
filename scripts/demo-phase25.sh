#!/usr/bin/env bash
# Phase 25 demo: one way to add an assistant, and the ones a machine has.
#
# 1. What this machine has: Codex's program on the PATH, ZCode's settings.
# 2. A new project with nobody at the terminal: Claude Code, and the
#    others named.
# 3. Said once, for every new project.
# 4. The same command for every assistant, in and out.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase25.sh
set -euo pipefail

export KITO_AUTHOR=ada
unset KITO_NO_CLAUDE
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
DEMO="$(mktemp -d)"
export HOME="$DEMO/home" KITO_CONFIG_DIR="$DEMO/config" KITO_REGISTRY="file://$DEMO/no-registry"
mkdir -p "$HOME/.zcode" "$DEMO/bin"
printf '#!/bin/sh\n' > "$DEMO/bin/codex" && chmod +x "$DEMO/bin/codex"
export PATH="$BIN:$DEMO/bin:/usr/bin:/bin"
trap 'for d in "$DEMO"/shop "$DEMO"/books; do kito --repo "$d" daemon stop >/dev/null 2>&1 || true; done; rm -rf "$DEMO"' EXIT

tidy() { sed -e "s#$DEMO#/demo#g"; }
run() { printf '\n$ %s\n' "$*" | tidy; "$@" 2>&1 | tidy || true; }

echo "== 1. What this machine has"
run kito assistant list

echo
echo "== 2. A new project, with nobody at the terminal"
mkdir -p "$DEMO/shop" && cd "$DEMO/shop"
run kito init

echo
echo "== 3. Said once, for every new project"
run kito assistant default claude
mkdir -p "$DEMO/books" && cd "$DEMO/books"
run kito init --no-claude

echo
echo "== 4. The same command for every assistant"
cd "$DEMO/shop"
run kito assistant remove claude
run kito assistant add claude
run kito assistant add codex
