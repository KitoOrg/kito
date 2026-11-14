#!/usr/bin/env bash
# How much the store grows when one line of a real source file is changed
# and saved again and again: the cost of keeping every save.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/store-growth.sh [source file] [saves] [directory to work in]
set -euo pipefail

# Kito alone: `kito init` would also set the project up for Claude Code.
export KITO_NO_CLAUDE=1

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$ROOT/target/release:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-ada}"
SOURCE="${1:-$ROOT/crates/kitod/src/repo.rs}"
SAVES="${2:-200}"
DIR="$(mktemp -d -p "${3:-${TMPDIR:-/tmp}}")"
trap 'kito --repo "$DIR" daemon stop >/dev/null 2>&1 || true; rm -rf "$DIR"' EXIT

mkdir -p "$DIR/src"
cd "$DIR"
cp "$SOURCE" src/file.txt.rs
kito init > /dev/null
kito sync > /dev/null
before=$(du -sb .kito/objects | cut -f1)
line=$(($(wc -l < src/file.txt.rs) / 2))
for i in $(seq 1 "$SAVES"); do
  sed -i "${line}s|.*|        // save $i|" src/file.txt.rs
  kito sync > /dev/null
done
after=$(du -sb .kito/objects | cut -f1)
size=$(wc -c < src/file.txt.rs)
echo "file of $((size / 1000)) kB, $SAVES saves of one line: the blobs grew by $(((after - before) / 1000)) kB, $(((after - before) / SAVES)) bytes per save"
du -sb .kito/objects .kito/oplog | awk '{ printf "  %s %.0f kB\n", $2, $1 / 1000 }'
