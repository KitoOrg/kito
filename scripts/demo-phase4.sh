#!/usr/bin/env bash
# Phase 4 demo: a function is renamed in one lane while another lane adds a
# new use of it under its old name.
#
# 1. git merges the two without a conflict, and the result does not compile.
# 2. Kito, with a language server: the rename reaches the new use by itself,
#    and the type checker and the tests pass before anything is integrated.
# 3. Kito, without one: the syntax alone is not sure, so it asks
#    (a `semantic_ref` conflict), and applies the answer.
#
# Needs `tsc` and `node` on the path; part 2 also needs
# `typescript-language-server` (npm install -g typescript-language-server).
#
#   cargo build --release -p kitod -p kito-cli
#   scripts/demo-phase4.sh
set -euo pipefail

# Kito alone: `kito init` would also set the project up for Claude Code.
export KITO_NO_CLAUDE=1

BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-francesco}"
export KITO_EMAIL="${KITO_EMAIL:-francesco@example.com}"
# A project that uses TypeScript has it in node_modules: here, a link to the
# one `tsc` belongs to.
TYPESCRIPT="$(dirname "$(dirname "$(readlink -f "$(command -v tsc)")")")"
TOP="$(mktemp -d)"
trap 'for d in "$TOP"/with-server "$TOP"/without; do kito --repo "$d" daemon stop >/dev/null 2>&1 || true; done; rm -rf "$TOP"' EXIT

run() { printf '\n$ %s\n' "$*"; "$@"; }
refused() { printf '\n$ %s\n' "$*"; "$@" || printf '(exit status %s)\n' "$?"; }

# The project: a function, a file that uses it, a test.
project() {
  mkdir -p "$1/src" "$1/node_modules"
  ln -s "$TYPESCRIPT" "$1/node_modules/typescript"
  printf '.kito/\nnode_modules/\n.out/\n' > "$1/.gitignore"
  printf '{ "compilerOptions": { "strict": true, "module": "commonjs", "target": "es2020", "rootDir": "src", "types": [] }, "include": ["src"] }\n' > "$1/tsconfig.json"
  cat > "$1/src/auth.ts" <<'TS'
export function getUser(id: string): string {
  return "user " + id;
}
TS
  cat > "$1/src/web.ts" <<'TS'
import { getUser } from "./auth";

export function profile(id: string): string {
  return "<h1>" + getUser(id) + "</h1>";
}
TS
  cat > "$1/src/test.ts" <<'TS'
import { profile } from "./web";

if (profile("7") !== "<h1>user 7</h1>") {
  throw new Error("profile is wrong");
}
console.log("tests passed");
TS
}

# What bob adds: a new file that calls the function by the name he knows.
billing() {
  cat > "$1/src/billing.ts" <<'TS'
import { getUser } from "./auth";

export function invoice(id: string): string {
  return "invoice for " + getUser(id);
}
TS
}

CHECKS='commands = ["tsc --noEmit -p .", "tsc -p . --outDir .out \&\& node .out/test.js"]'

printf '# 1. With git: alice renames getUser to fetchUser, bob adds a file that uses getUser\n'
G="$TOP/git"
project "$G"
(
  cd "$G"
  export GIT_AUTHOR_NAME=demo GIT_AUTHOR_EMAIL=demo@example.com GIT_COMMITTER_NAME=demo GIT_COMMITTER_EMAIL=demo@example.com
  git init -q -b main .
  git add -A && git commit -q -m start
  git checkout -q -b alice
  sed -i 's/getUser/fetchUser/g' src/auth.ts src/web.ts
  git commit -q -am "rename getUser to fetchUser"
  git checkout -q main && git checkout -q -b bob
  billing .
  git add -A && git commit -q -m "billing"
  git checkout -q main
  run git merge -q --no-edit alice
  run git merge --no-edit bob
  refused tsc --noEmit -p .
)

kito_part() {
  local dir="$1" servers="$2"
  project "$dir"
  cd "$dir"
  run kito init
  sed -i "s|^# commands = .*|$CHECKS|; s|^# share = .*|share = [\"node_modules\"]|" .kito/config.toml
  if [ "$servers" = none ]; then
    # No language server at all: only what the syntax tells.
    sed -i '/^\[semantic.servers/,$d' .kito/config.toml
    printf '[semantic]\nservers = {}\n' >> .kito/config.toml
  fi
  kito daemon stop > /dev/null
  run kito checkpoint -m "start"
  run kito lane open alice --task "rename getUser to fetchUser"
  run kito lane open bob --task "billing"
}

if command -v typescript-language-server > /dev/null; then
  printf '\n\n# 2. With Kito and a language server (typescript-language-server)\n'
  kito_part "$TOP/with-server" default
  printf '\n# alice renames the function: its uses in her lane follow\n'
  run kito rename src/auth.ts:getUser fetchUser --lane lane/1
  printf '\n# bob, who knows nothing of it, adds his file\n'
  billing "$PWD/.kito/lanes/2/work"
  run kito integrate lane/1
  run kito integrate lane/2
  run cat src/billing.ts
  run kito log -n 2
  run kito why src/auth.ts:fetchUser
  run git log --oneline
  kito daemon stop > /dev/null
else
  printf '\n\n# 2. skipped: typescript-language-server is not installed\n'
fi

printf '\n\n# 3. With Kito and no language server: it asks\n'
kito_part "$TOP/without" none
printf '\n# alice renames by hand, everywhere she finds the name; bob adds his file\n'
sed -i 's/getUser/fetchUser/g' .kito/lanes/1/work/src/auth.ts .kito/lanes/1/work/src/web.ts
billing "$PWD/.kito/lanes/2/work"
run kito integrate lane/1
refused kito integrate lane/2
CONFLICT="$(kito conflicts --json | sed -n 's/.*"id": *"\(conflict#[0-9]*\)".*/\1/p' | head -n 1)"
run kito conflicts "$CONFLICT"
run kito resolve "$CONFLICT" --rewrite
run kito integrate lane/2
run cat src/billing.ts
run git log --oneline
