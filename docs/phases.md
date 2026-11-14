# Phase status

One phase at a time: each one ends with green tests and a runnable demo.

## Phase 0 · foundations — done

`kito-core` and `kito-store`: append-only oplog with CRC, BLAKE3 + zstd
objects, rebuildable indexes.

### What is there

- **Data model** (`kito-core`): the plan's types, with IDs and hashes;
  canonical CBOR encoding; `OpId` = BLAKE3 of (parents, kind, author); HLC
  clock. A "golden" test pins the bytes and the ID of a sample op: if it
  changes, the on-disk format has changed and an ADR is needed.
- **Segmented log** (`kito-store`): `length + payload + CRC32C` records,
  64 MiB segments created atomically (write, fsync, rename), a single writer
  protected by a lock.
- **Blobs**: addressed by BLAKE3, compressed with zstd when it pays off,
  deduplicated, verified against the hash on every read.
- **Indexes** (redb): op → position and lane, lane → op, symbol → op, blob →
  position. Always rebuildable from the logs.
- **Recovery**: on open the torn tail is discarded (and kept in
  `lost+found/`), and the index is caught up. Damage to data already
  confirmed is never hidden: it is an error, which can be overridden only in
  repair mode.
- **`Store::verify`**: full check of CRCs, hashes, IDs, parents, indexes.

### Demo

Write 1 million ops, `kill -9` at random moments, restart and verify that no
confirmed op is lost.

```bash
cargo run --release -p kito-store --example crash_demo -- driver --dir /tmp/kito-demo --ops 1000000
```

Result of the run of 14 November 2026 (seed 20261114, output in
[`demo/phase0-crash-demo.txt`](demo/phase0-crash-demo.txt)):

| | |
| --- | --- |
| Ops written and verified one by one | 1,000,000 |
| `kill -9` | 228 |
| …after a commit | 99 |
| …at startup, including during recovery | 43 |
| …in the middle of a write | 86 |
| Torn tails discarded | 43 (124,880 bytes, never confirmed) |
| Index caught up after a kill | 125 times |
| Confirmed ops lost | 0 |
| Writing time, kills included | 130 s |

At the end the demo rereads all the ops and blobs, runs `verify` and rebuilds
the index from scratch, checking that it answers like the incremental one.

### Tests

- Properties (`proptest`): records survive any truncation and detect any
  flipped bit; the encoding of ops is deterministic.
- Model with simulated power loss: random sequences of writes, commits,
  crashes, failed fsyncs; the unsynced bytes are lost, truncated, zeroed or
  garbled. After every reopen the store contains a prefix of the history with
  at least all the confirmed ops.
- Fault injection: truncation at every byte of the tail, garbage at the end
  of files, flipped bits in sealed and active segments, missing segments,
  disk full at different points, failed fsync, partial write that cannot be
  undone, index missing, unreadable, stale or from another store.

### Acceptance criteria touched by this phase

| Metric | v1 target | Status |
| --- | --- | --- |
| Crash recovery | No confirmed op lost | verified by the demo and the tests |

Latencies are measured from phase 1 on, when the full save → op in the log
path exists. For reference, the store alone takes about 0.6 ms to confirm an
op with two blobs (see [`benchmarks.md`](benchmarks.md)).

### Known limitations

- The demo uses `kill -9`, which does not lose the operating system's cache:
  power loss is covered only by the simulation in the tests.
- See D-007 and D-011 in [`decisions.md`](decisions.md).

## Phase 1 · continuous textual capture — done

`kitod` with watcher; `TextEdit` and `File*` ops; checkpoints; `kito log`;
export to git and import from git.

### What is there

- **Capture** (`kitod`): a watcher on the working directory collects the
  events, waits for 50 ms of quiet and turns the state of the files into
  chained ops on the main lane. A full scan reconciles at startup, every
  60 seconds and when the watcher reports lost events; it uses a cache of
  size and modification date to avoid rereading unchanged files.
- **Textual ops** (`kito-diff`): addition, removal, rename (only if the
  content is identical) and modification with line-level edits.
- **Ignore rules**: `.gitignore` files, nested ones included,
  `.git/info/exclude`, `.kitoignore` and the configuration; the same for
  watcher and scan.
- **Checkpoints**: labels on a point of the log, created by hand.
- **Git** (`kito-git`): every checkpoint becomes a deterministic commit; the
  branch advances only by fast-forward and git's index is realigned, so
  `git status` stays clean. Import brings in the history of an existing
  repository.
- **Daemon and CLI**: Unix socket with JSON-RPC, a single daemon per
  repository, started automatically by the CLI. Commands: `init`, `import`,
  `log`, `checkpoint`, `export-git`, `sync`, `status`,
  `daemon status|start|stop`; all with `--json`.

### Demo

You work in a directory without committing, `kito log` shows everything,
`git log` sees the checkpoints.

```bash
cargo build --release -p kitod -p kito-cli
```

```bash
scripts/demo-phase1.sh
```

Output of the run of 14 November 2026 in
[`demo/phase1-demo.txt`](demo/phase1-demo.txt). In short: three saves with no
commit at all show up in `kito log` as `FileAdd` and `TextEdit`; two
`kito checkpoint`s become two commits in `git log`; `git status` is clean and
`git fsck --strict` passes.

### Acceptance criteria rechecked

The plan asks to recheck the targets with real measurements at the end of
this phase. The only one measurable now is the latency of a save; lane
creation and integration arrive with phase 3.

Saving a 2,000-line file (172 kB), 300 saves, real daemon:

| Measurement | p50 | p95 | v1 target |
| --- | --- | --- | --- |
| Processing after the debounce (read, hash, diff, two fsyncs) | 3.1 ms | 4.7 ms | |
| From the first event to the confirmed op, debounce included | 53.8 ms | 55.5 ms | |
| From the save to the confirmed op, as seen by the client | 54.9 ms | 56.7 ms | < 30 ms at p95 |

**The target cannot be met as written**: the plan sets a debounce of 50 ms,
which alone exceeds 30 ms. The actual work costs less than 5 ms. There are
two ways forward, and the choice belongs to whoever owns the plan: read the
target as "after the debounce" (already met by a wide margin), or lower
`debounce_ms` to 20 or less in `.kito/config.toml` (the total latency drops
to about 25 ms, at the price of a few extra ops when an editor saves in
several steps). The default stays 50 ms.

The measurement can be repeated with:

```bash
cargo run --release -p kitod --example latency -- --saves 300 --lines 2000
```

### Tests

- Capture engine: every kind of change on disk, per-path and per-directory
  events, ignore rules, same-size rewrites, restart after a crash, changes
  made while the daemon was off.
- Real daemon: capture without explicit requests, bursts of saves, `sync` as
  a barrier, restart, a single daemon per repository, `kill -9` during
  capture.
- Git: export verified with `git fsck --strict` and compared file by file
  with Kito's state, also on random sequences of states; continuity with a
  pre-existing git history; detection of commits made outside Kito; import
  with renames, permissions, merges.
- CLI: the real commands, in human-readable and JSON format.

### Known limitations

- Import follows first parents only: merged branches show up as the effect
  of the merge commit (D-018).
- Commits made directly in git after the import are not yet absorbed back:
  the export stops with a clear error (D-017).
- Intents (the "why" of the ops) arrive with phase 3, together with the MCP
  tool that sets them.
- Linux and macOS only, as per the plan's default; tested on Linux.
- Where file events do not arrive (for example `/mnt/c` under WSL) capture
  goes through scanning: complete, but with a delay of about one second and
  without the intermediate saves (D-025).

## Phase 2 · level 1 semantics — done

`kito-parse` with tree-sitter for the ten languages; `kito-diff` with stable
symbol IDs; symbolic ops in the capture path; `kito why`.

### What is there

- **Parsing** (`kito-parse`): one tree-sitter grammar and one query file per
  language (`queries/<lang>/symbols.scm`) for Python, JavaScript, TypeScript
  (and TSX), Java, C#, C, C++, Go, PHP and Rust. Each symbol gets a kind, a
  name, its place in the nesting, and three hashes of its tokens (signature,
  body, and a MinHash fingerprint) that ignore whitespace, comments and the
  symbol's own name.
- **Stable IDs** (`kito-diff`): old and new symbols are matched in passes —
  same name; same body; resemblance — scope by scope, then across scopes and
  files for the moves. A symbol keeps its ID when it is renamed, edited,
  moved to another file, moved to another class or namespace, or all of
  these at once.
- **Symbolic ops in the log**: every save is recorded as a file-level op
  (which carries the content, so the state never depends on a parser),
  followed by the symbolic ops that say what it meant: `AddSymbol`,
  `RemoveSymbol`, `Rename`, `Move`, `ModifySignature`, `ModifyBody`. A
  reformat or a comment produces none.
- **`kito why <file>:<symbol>`**: the whole story of a symbol, asked by the
  name and the place it has today — including the moves it made inside its
  class and the renames of its file.
- **`kito log`** shows under each change what it meant for the symbols;
  `kito status` counts them; rollback and git export keep working unchanged.
- **Incremental parsing**: the daemon keeps the syntax trees of the files it
  captured last, so a save reparses only what changed.

### Demo

A function is renamed, moved to another file and edited there; a method
leaves its class for another one. `kito why` follows each of them; git, for
the same saves, sees lines removed here and added there.

```bash
cargo build --release -p kitod -p kito-cli
```

```bash
scripts/demo-phase2.sh
```

Output of the run of 14 November 2026 in
[`demo/phase2-demo.txt`](demo/phase2-demo.txt). The heart of it:

```text
$ kito why src/users.ts:fetchUser
function fetchUser  (src/users.ts)  symbol 0#0
  2026-11-14 15:24:52  francesco  function `getUser` created in src/auth.ts
  2026-11-14 15:24:52  francesco  renamed `getUser` → `fetchUser`
  2026-11-14 15:24:52  francesco  moved from src/auth.ts to src/users.ts
  2026-11-14 15:24:52  francesco  body changed (+1 −1)
```

### Acceptance criteria rechecked

**Renames and moves recognized on the test corpus: target ≥ 99%.** The corpus
is one open-source repository per language, at a pinned tag (`requests`,
`express`, `zod`, `gson`, `Newtonsoft.Json`, `jq`, `leveldb`, `cobra`,
`guzzle`, `ripgrep`): 1,183 files, 16,375 symbols. Git history does not say
which functions were renamed, so the ground truth is made by mutation: each
sampled symbol is in turn renamed (every whole-word occurrence in its file,
as a refactoring tool does), moved to another file, and replaced by a
different symbol taken from another file. A mutation that leaves a syntax
error is discarded.

| Language | Files | Symbols | Renames kept | …of small symbols | Renamed and edited | …of small symbols | Moves kept | …out of a namespace | Different symbols told apart |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| python | 32 | 853 | 100.0% (125/125) | 100.0% (27/27) | 98.1% (103/105) | 52.6% (10/19) | 100.0% (68/68) | — | 100.0% (71/71) |
| javascript | 41 | 151 | 100.0% (89/89) | 100.0% (4/4) | 100.0% (78/78) | 0.0% (0/3) | 100.0% (83/83) | — | 100.0% (71/71) |
| typescript | 143 | 2102 | 100.0% (230/230) | 100.0% (98/98) | 92.1% (116/126) | 12.5% (6/48) | 100.0% (206/206) | 100.0% (30/30) | 100.0% (66/66) |
| java | 243 | 3933 | 100.0% (1093/1093) | 100.0% (217/217) | 99.3% (896/902) | 67.6% (117/173) | 100.0% (167/167) | — | 99.8% (955/957) |
| csharp | 400 | 3378 | 100.0% (1250/1250) | 100.0% (285/285) | 98.4% (970/986) | 83.3% (165/198) | 100.0% (330/330) | 100.0% (330/330) | 99.7% (903/906) |
| c | 51 | 881 | 100.0% (119/119) | 100.0% (32/32) | 100.0% (51/51) | 11.9% (5/42) | 100.0% (110/110) | — | 100.0% (45/45) |
| cpp | 73 | 917 | 100.0% (237/237) | 100.0% (27/27) | 98.6% (208/211) | 9.5% (2/21) | 100.0% (113/113) | 100.0% (69/69) | 100.0% (106/106) |
| go | 36 | 650 | 100.0% (147/147) | 100.0% (7/7) | 99.3% (136/137) | 0.0% (0/6) | 100.0% (147/147) | — | 100.0% (133/133) |
| php | 71 | 799 | 100.0% (319/319) | 100.0% (67/67) | 97.3% (251/258) | 88.1% (52/59) | 100.0% (77/77) | 100.0% (2/2) | 98.6% (218/221) |
| rust | 93 | 2711 | 100.0% (414/414) | 100.0% (46/46) | 98.7% (389/394) | 18.2% (2/11) | 100.0% (209/209) | 100.0% (49/49) | 100.0% (238/238) |
| **all** | 1183 | 16375 | 100.0% (4023/4023) | 100.0% (810/810) | 98.5% (3198/3248) | 61.9% (359/580) | 100.0% (1510/1510) | 100.0% (480/480) | 99.7% (2806/2814) |

- Pure renames and pure moves are recognized in every case tried: **100%**
  (4,023 renames, 1,510 moves), above the target.
- *Renamed and edited* is harder than what the criterion asks: the name and
  one identifier of the body change in the same save. With enough text to
  have a fingerprint it is recognized 98.5% of the time; a symbol of a few
  tokens is not expected to survive it (61.9%), and when it does not the
  cost is an add and a remove, never a wrong identity.
- The eight look-alikes mistaken for the same symbol (0.3%) are near-copies:
  sibling test methods and classes with the same members.

The measurement can be repeated with:

```bash
scripts/fetch-corpus.sh
```

```bash
cargo run --release -p kito-diff --example corpus -- target/corpus
```

**Latency of a save**, now that every save is also parsed and compared.
300 saves with the real daemon, one line changed each time:

| File | Processing after the debounce, p50 | p95 | Save → confirmed op, p95 |
| --- | --- | --- | --- |
| 2,000 lines, 143 functions (58 kB): an ordinary source file | 14.1 ms | 20.9 ms | 72.2 ms |
| 2,000 lines, 2,000 one-line functions (172 kB): the worst case | 31.5 ms | 50.5 ms | 102.2 ms |

The semantic level costs about 10 ms per save on an ordinary file. The
"< 30 ms at p95" target is met by the work itself on ordinary files and
missed once the 50 ms debounce is added, as in phase 1: the open point is the
same and is not made worse by a decision of this phase. Most of the remaining
cost is re-reading the symbols of the whole file after an incremental parse;
reusing the ones that did not change is a phase 5 optimization.

```bash
cargo run --release -p kitod --example latency -- --shape typical
```

### Tests

- Parsing: the outline of a sample file per language (kinds, names,
  nesting); hashes unchanged by reformatting, comments and renames, also
  when a symbol mentions itself; incremental parsing equal to parsing from
  scratch on random edits.
- AST differ: edits, renames, renames with an edit, overloads, look-alikes
  that must not be confused, moves between files, a class leaving its
  namespace, a file split in two, a method changing class, nesting turned
  inside out; random sequences of edits with the table compared to the
  parser after every step.
- Capture: the demo scenario end to end, half-written code, rollback and git
  export with symbols, the first analysis of an imported repository, restart
  from the saved state and from the log alone.
- CLI: `kito why` and the log with the real binaries.

### Known limitations

- Fields and local variables are not symbols; top-level constants and
  variables are (D-031).
- A symbol that changes kind is a new symbol: a method that becomes a free
  function keeps its history only as text (D-033).
- A method that follows its class to another file has no `Move` of its own:
  `kito why` reads it from the class. A file renamed and renamed back leaves
  no trace in `kito why` (D-035).
- Explicit renames through MCP (`kito.rename_symbol`), which skip the
  heuristics, arrive with the MCP server in phase 3.
- References between symbols (who calls whom) are level 2: phase 4.

## Phase 3 · lanes and integration — to do

## Phase 4 · level 2 semantics — to do

## Phase 5 · coverage and performance — to do
