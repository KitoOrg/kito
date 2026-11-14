#!/usr/bin/env bash
# Phase 5 demo: the acceptance criteria of the plan, measured on a
# repository of 50,000 files, one after the other, with a verdict each.
#
#   cargo build --release -p kitod -p kito-cli
#   cargo build --release -p kitod -p kito-store -p kito-diff --examples
#   scripts/fetch-corpus.sh          # once, for the last criterion
#   scripts/demo-phase5.sh [directory to work in]
#
# The directory is where the test repository is created: its file system is
# the one the measurements are about (default: the system's temporary one).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EX="$ROOT/target/release/examples"
WORK="${1:-${TMPDIR:-/tmp}}"
FILES="${KITO_DEMO_FILES:-50000}"
OPS="${KITO_DEMO_CRASH_OPS:-200000}"
export PATH="$ROOT/target/release:$PATH"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

verdicts=()
verdict() { # criterion, target, measured, met|not met|not measured
  verdicts+=("| $1 | $2 | $3 | $4 |")
}
below() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a + 0 < b + 0) }'; }
section() { printf '\n\n# %s\n\n' "$1"; }

section "1. From a save to its op: under 30 ms at the 95th percentile, file of 2,000 lines"
"$EX/latency" --saves 300 --lines 2000 | tee "$OUT/latency"
P95="$(sed -n 's/^| Save → confirmed op, as seen by the client | [0-9.]* ms | \([0-9.]*\) ms.*/\1/p' "$OUT/latency")"
verdict "Save → op, p95, 2,000-line file" "< 30 ms" "$P95 ms" "$(below "$P95" 30 && echo met || echo 'not met')"

section "2 and 3. Creating a lane and integrating one, on $FILES files"
"$EX/scale" --files "$FILES" --git-export --dir "$WORK" | tee "$OUT/scale"
LANE="$(sed -n 's/^| median | \([0-9]*\) ms |/\1/p' "$OUT/scale")"
BEST="$(sed -n 's/^| best | \([0-9]*\) ms |/\1/p' "$OUT/scale")"
verdict "Creating a lane, $FILES files (median of 5; best $BEST ms)" "< 1 s, with copy-on-write" "$LANE ms, files copied" \
  "$(below "$LANE" 1000 && echo 'met, without copy-on-write' || echo 'not met here: no copy-on-write on this file system')"
WORST="$(sed -n 's/^| lane [0-9]*: .* (\([0-9]*\) ms per 100 ops) |/\1/p' "$OUT/scale" | sort -n | tail -n 1)"
FIRST="$(sed -n 's/^| lane 1: .* (\([0-9]*\) ms per 100 ops) |/\1/p' "$OUT/scale")"
verdict "Integrating without conflicts, verifier excluded, git commit included (worst of 5 lanes; $FIRST ms for a lane that is up to date)" \
  "< 200 ms per 100 ops" "$WORST ms per 100 ops" "$(below "$WORST" 200 && echo met || echo 'not met')"

section "4. No confirmed op lost after a crash: $OPS ops, kill -9 at random moments"
rm -rf "$WORK/kito-demo5-crash"
"$EX/crash_demo" driver --dir "$WORK/kito-demo5-crash" --ops "$OPS" | tail -n 12 | tee "$OUT/crash"
rm -rf "$WORK/kito-demo5-crash"
LOST="$(sed -n 's/^confirmed ops lost \.* \([0-9]*\)$/\1/p' "$OUT/crash")"
KILLS="$(sed -n 's/^verifications after a kill \.* \([0-9]*\)$/\1/p' "$OUT/crash")"
verdict "Confirmed ops lost after a crash ($KILLS kills)" "0" "${LOST:-?}" "$([ "${LOST:-1}" = 0 ] && echo met || echo 'not met')"

section "5. No fake conflicts: ten agents, four rounds, every integration verified"
"$EX/swarm" --agents 10 --rounds 4 | tee "$OUT/swarm"
FAKE="$(sed -n 's/^| Fake conflicts (changes to different symbols) | \([0-9]*\) |/\1/p' "$OUT/swarm")"
DONE="$(sed -n 's/^| Integrated | \([0-9]*\) |/\1/p' "$OUT/swarm")"
verdict "Fake conflicts ($DONE integrations by 10 agents)" "0" "${FAKE:-?}" "$([ "${FAKE:-1}" = 0 ] && echo met || echo 'not met')"

section "6. Renames and moves recognized: at least 99% on the corpus"
if [ -d "$ROOT/target/corpus" ]; then
  "$EX/corpus" "$ROOT/target/corpus" | tee "$OUT/corpus" | tail -n 6
  ALL="$(grep '^| \*\*all\*\*' "$OUT/corpus")"
  RENAMES="$(printf '%s' "$ALL" | awk -F'|' '{ print $5 }' | sed 's/^ *//; s/ (.*//')"
  MOVES="$(printf '%s' "$ALL" | awk -F'|' '{ print $9 }' | sed 's/^ *//; s/ (.*//')"
  LOW="$(printf '%s\n%s\n' "${RENAMES%\%}" "${MOVES%\%}" | sort -n | head -n 1)"
  verdict "Renames / moves kept on the corpus (ten repositories)" ">= 99%" "$RENAMES / $MOVES" \
    "$(below 98.999 "$LOW" && echo met || echo 'not met')"
else
  printf 'The corpus is not here: run scripts/fetch-corpus.sh\n'
  verdict "Renames / moves kept on the corpus" ">= 99%" "—" "not measured: run scripts/fetch-corpus.sh"
fi

section "The acceptance criteria"
printf '| Criterion | Target | Measured | |\n| --- | --- | --- | --- |\n'
printf '%s\n' "${verdicts[@]}"
