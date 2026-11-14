#!/usr/bin/env bash
# Fetches the test corpus: one open-source repository per language, each
# pinned to a release tag so that the measurements can be repeated.
#
#   scripts/fetch-corpus.sh [directory]     (default: target/corpus)
set -euo pipefail

DEST="${1:-target/corpus}"
mkdir -p "$DEST"

fetch() { # language, repository, tag
  if [ -d "$DEST/$1/.git" ]; then
    echo "$1: already there"
    return
  fi
  echo "$1: $2 at $3"
  git clone --quiet --depth 1 --branch "$3" "https://github.com/$2.git" "$DEST/$1"
}

fetch python     psf/requests            v2.32.3
fetch javascript expressjs/express       4.21.2
fetch typescript colinhacks/zod          v3.23.8
fetch java       google/gson             gson-parent-2.11.0
fetch csharp     JamesNK/Newtonsoft.Json 13.0.3
fetch c          jqlang/jq               jq-1.7.1
fetch cpp        google/leveldb          1.23
fetch go         spf13/cobra             v1.8.1
fetch php        guzzle/guzzle           7.9.2
fetch rust       BurntSushi/ripgrep      14.1.1
