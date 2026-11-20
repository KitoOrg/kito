#!/usr/bin/env bash
# Phase 18 demo: the why graph, better.
#
# 1. An intent that says nothing is refused.
# 2. A note on a function: something to know that no change says. It is
#    told with the why of that code, and follows the function.
# 3. A file renamed keeps the reasons of its lines.
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase18.sh
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

mkdir -p "$APP/src"; cd "$APP"
cat > src/billing.py <<'PY'
def invoice(customer, amount):
    total = amount
    return {"customer": customer, "total": total}
PY
printf '.kito/\n' > .gitignore
kito init . > /dev/null
kito checkpoint -m "invoices" > /dev/null

printf '# 1. An intent that says nothing is refused\n'
run kito lane open agent-a --intent "wip" --scope src/billing.py || true
run kito lane open agent-a --intent "taxes on every invoice" --scope src/billing.py
cat > .kito/lanes/1/work/src/billing.py <<'PY'
def invoice(customer, amount):
    total = round(amount * 1.2, 2)
    return {"customer": customer, "total": total}
PY
run kito integrate 1
kito lane close 1 > /dev/null

printf '\n\n# 2. A note on code\n'
run kito note src/billing.py:invoice "called by the nightly billing job, outside this repository: do not rename"
run kito why src/billing.py:invoice

printf '\n\n# 3. A renamed file keeps the reasons of its lines\n'
mv src/billing.py src/invoices.py
run kito checkpoint -m "billing becomes invoices"
run kito why src/invoices.py:2
run kito note --list
