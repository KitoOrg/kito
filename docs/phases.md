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

## Phase 3 · lanes and integration — done

`kito-lane` with copy-on-write working copies; `kito-merge` with every
composition rule except `SemanticRef`; `kito-integrate` with the queue and
the verifier; `kito-mcp`, the MCP server; a swarm simulator.

### What is there

- **Lanes** (`kito lane open <agent>`): an agent gets a working copy of its
  own, `.kito/lanes/<id>/work`, cloned from the main directory where the
  file system shares data between files (reflink) and copied elsewhere.
  Every save there is captured exactly as in the main directory — file ops
  and symbolic ops, signed by the agent — on the lane's own log. What the
  tools need and is not tracked (`node_modules`, `.venv`) is seen through a
  link: `[lanes] share` in the configuration.
- **Semantic rebase** (`kito-merge`): integrating a lane means composing
  what it did since its base with what the main lane has become, symbol by
  symbol. A file is merged as what it is made of — a header, its symbols in
  order, a tail — and so is every symbol that contains others.

  | On the main lane | On the lane | Result |
  | --- | --- | --- |
  | change to symbol X | change to symbol Y | both apply, wherever they are in the file |
  | new symbol at some place | another new symbol at the same place | both stay, in an order that does not depend on who integrates first |
  | change to X | change to X | merged line by line; if the same lines changed, conflict `body_body` |
  | rename of X | change to X | both apply |
  | rename of X | another rename of X | conflict `rename_rename` |
  | removal of X | change to X | conflict `remove_modify` |
  | X moved to another file, class or namespace | change to X | both apply: the change follows the symbol |
  | text outside the symbols, files without a grammar | the same | three-way merge of the lines; if it fails, conflict `text` |

- **Integration** (`kito integrate lane/1 …`): the merged files are written
  in the main directory and recorded there as ops signed by the lane's
  agent; the first has two parents, the head of the main lane and the head
  of the lane. Symbols keep the IDs the lane gave them. Every integration
  is a checkpoint, exported as a git commit with the agent as its author.
  The lane then continues from the new state of the main lane.
- **Queue and verifier**: the daemon integrates one lane at a time. Before
  anything is written, the merged result is checked in a copy of its own
  (`.kito/verify/work`) by the commands of `[verify] commands` — a type
  checker, the tests — each with a time limit. If one fails the lane is not
  integrated and gets the report. The repository is not held while they
  run: saves keep being recorded.
- **Conflicts** are recorded with the three texts (starting point, main
  lane, lane) and assigned to the lane's agent. `kito conflicts` lists
  them, `kito resolve <conflict> --main | --lane | --text FILE` decides one;
  the next integration of the lane applies the decisions, provided the
  texts they were taken on have not changed.
- **MCP server** (`kito-mcp`, on standard input and output): the tools of
  the plan — `kito.open_lane`, `kito.sync`, `kito.status`, `kito.log`,
  `kito.set_intent`, `kito.rename_symbol`, `kito.request_integration`,
  `kito.integration_result`, `kito.why`, `kito.resolve_conflict`,
  `kito.close_lane`. An agent asks for an integration, gets a job, and
  reads the result when it is ready:

  ```json
  {
    "lane": "lane/2",
    "result": "integrated",
    "checkpoint": "ckpt#3",
    "git_commit": "824ccb29795a983219cbe3b94653849fe1a01d3f",
    "auto_resolutions": [
      { "kind": "body_merge", "symbol": "ts/m.a#0", "locations": ["src/m.ts"] }
    ],
    "conflicts": [],
    "verification": { "commands": ["npx tsc --noEmit"], "passed": true, "duration_ms": 1312 },
    "your_next_base": "ckpt#3"
  }
  ```

- **Intents and explicit renames**: `kito intent "why" [--task …]` (or
  `kito.set_intent`) labels the ops recorded from then on. The checkpoint
  of an integration is named after the lane's task or, without one, after
  its intent. `kito rename <file:symbol> <new name>`
  (or `kito.rename_symbol`) renames a symbol and records the rename as such,
  without leaving it to the heuristics.
- **Command line**: `kito lanes`, `kito lane open | close | status`,
  `kito integrate`, `kito conflicts`, `kito resolve`, `kito rename`,
  `kito intent`; `kito log`, `kito why` and `kito sync` take a lane. `--json`
  prints what the MCP server returns.
- **Swarm simulator** (`kitod::simulate`): agents that work at the same
  time, through the real daemon, on a project where every one of them
  touches every file. It knows which conflicts are real because it stages
  them, and fails on any other.

### Demo

Two agents add a function at the same place of the same file and fix two
lines of the same function: git reports a conflict, Kito integrates both.
Then the same line changed in two ways: a real conflict, reported with the
three texts, decided and integrated. Then ten simulated agents, four rounds
each, every integration checked by the TypeScript compiler.

```bash
cargo build --release -p kitod -p kito-cli
```

```bash
cargo build --release -p kitod --example swarm
```

```bash
scripts/demo-phase3.sh
```

Output of the run of 14 November 2026 in
[`demo/phase3-demo.txt`](demo/phase3-demo.txt). The heart of it:

```text
$ git merge-file -p alice/src/shop.ts src/shop.ts bob/src/shop.ts | tail -n 8
<<<<<<< alice
export function tax(amount: number): number {
  return Math.round(amount * 0.22);
=======
export function shipping(weight: number): number {
  return weight > 1000 ? 900 : 500;
>>>>>>> bob
}
(git merge-file: 1 conflict)

$ kito integrate lane/1 lane/2
lane/1: integrated as ckpt#2 (git 883d187613)
lane/2: integrated as ckpt#3 (git 78a383c445)
  merged by itself: ts/shop.total#0
```

and, from the swarm:

```text
| Integrations asked | 40 |
| Integrated | 40 |
| Fake conflicts (changes to different symbols) | 0 |
| Real conflicts staged / found and settled | 3 / 3 |
| Kinds of the real conflicts | rename_rename, remove_modify, body_body |
| Verification failures | 0 |
| Checkpoints (one per integration, each a git commit) | 46 |

git fsck --strict: ok;  git status: clean
tsc on the main lane after the swarm: no errors
```

The median of 12 s per integration in that run is the queue: ten agents ask
at the same moment and each verification is a run of `tsc` (1.3 s).

### Acceptance criteria rechecked

**0 fake conflicts.** In the simulator every agent edits a function of its
own in every file, adds functions where the others add theirs, and renames
and moves its symbols: 40 integrations out of 40 without a conflict, and
the 3 conflicts staged on purpose found, of the 3 kinds. A property test
checks that lanes working on different symbols give the same files in
whatever order they are integrated. The first runs of the simulator did
find fake conflicts, and their cause was in the capture, not in the merge:
see D-047.

**Integration without conflicts under 200 ms per 100 ops, verifier
excluded.** Measured on a repository of 50,000 files (250,000 symbols),
lanes that change one function per file:

| Lane | Ops | Time | Per 100 ops |
| --- | --- | --- | --- |
| 50 files changed, the main lane has not moved | 100 | 90 ms | 90 ms |
| 500 files changed | 1,000 | 299 ms | 30 ms |
| 50 files changed, the main lane is 550 to 650 files ahead | 100 | 123–136 ms | 123–136 ms |

These are without the git commit of each integration. With it, as by
default, the three rows take 135–145 ms, 447–511 ms and 185–245 ms: the
target is met for a lane that is up to date and is at the limit for one
that is 600 files behind, whose working copy is rewritten in the same
time. On 5,000 files the first two rows take 37 ms and 317 ms. The first
version took 2.1 s for the first row (and a second more for the commit);
what changed is in [`benchmarks.md`](benchmarks.md).

```bash
cargo run --release -p kitod --example scale -- --files 50000
```

**Creating a lane under 1 s on 50,000 files (copy-on-write).** Not shown as
the plan states it: the disk of the test machine is ext4, which cannot share
data between files, so every file is copied; no file system with reflinks
was available to measure the other case. With copies, a lane on 50,000 files
(1 kB each) took 462–561 ms (median 490 ms) in a quiet run and up to 2 s in
others. The difference is the machine, not Kito: on this VM creating an
empty file costs between 13 and 250 µs depending on what the file system has
in memory. On the 15 files of the demo a lane opens in 2 ms.

**No confirmed op lost after a crash.** The logs of the lanes and of the
conflicts are the same checksummed append-only logs as the op log, and what
a lane has is rebuilt from the log alone: a test restarts in the middle of a
lane's work with the saved states removed.

**Latency of a save.** Unchanged: the phase 2 code and this one, measured
one after the other, give the same figures (processing p50 13 ms, p95
20 ms on the ordinary file).

**Git round trip.** Every integration is a commit; `git fsck --strict` and
a clean `git status` are checked after the swarm, in the demo and in the
tests.

### Tests

202 in the workspace. New in this phase:

- Merge: one scenario per rule of the table above, in both directions;
  moves between files, classes and namespaces while the other side edits;
  renamed and removed files; a lane that stayed behind reads only the files
  tied to its work; the property test on the order of integration.
- Lanes: working copies with content, permissions and links; independence
  from the main directory; a file that changes while it is copied is not
  used; capture on several lanes at once.
- Integration: the three steps, a failed verification that leaves the main
  lane untouched, a verification that outlives its time limit, conflicts
  recorded, decided and applied, decisions that no longer apply, restart in
  the middle.
- Daemon, command line and MCP server end to end with the real binaries.
- The simulator: 6 agents for 4 rounds through the real daemon on every run
  of the tests.

### Known limitations

- **No `SemanticRef` in this phase** (it came with phase 4): a lane that
  renames a function does not update the calls another lane adds
  meanwhile. Two lanes that add a function with the same name both keep
  theirs, then as now: it is the verifier (a type checker) that stops the
  integration.
- An explicit rename changes the definition and the uses inside the symbol
  itself; the other places where the name appears are listed, not edited
  (until phase 4).
- A conflict is about a whole symbol (or a whole file, for text): it is
  decided by choosing a side or giving the new text, not hunk by hunk.
- A reordering is not an op: when both sides changed a file, its symbols
  stay in the order the main lane has.
- An integration is a squash: the lane's own ops stay in its log, where
  `kito why` and `kito log --lane` find them, and git gets one commit.
- Without reflinks a lane costs a copy of the repository, in time and in
  disk space.
- A lane's working copy is inside the repository's directory and has no
  `.git` of its own: git commands run there see the main repository.
- The saved states are written when the daemon stops. After a crash nothing
  is lost, but every lane rereads its files once (D-048).
- One integration at a time: with slow verification commands, lanes wait.

## Phase 4 · level 2 semantics — done

`kito-semantic` with the `SemanticProvider` trait, the provider that reads
the syntax and the one that asks a language server (tried with TypeScript
and Go); `SemanticRef` conflicts; renames that reach the uses of the symbol.

### What is there

- **References between symbols** (`kito-semantic`): a provider answers two
  questions about a state of the repository — which references leave the
  symbols of a file, and who uses a symbol — and every answer carries how
  sure it is and where it comes from.
  - `TreeSitterHeuristic` is always there and reads only the syntax: the
    uses in the file that defines the symbol, and the ones that reach it
    through an explicit import (JavaScript and TypeScript, Python, Go). It
    is never sure: 0.5 to 0.8.
  - `LspProvider` asks a language server, and is as sure as the server.
    One server per language runs in the main directory, started the first
    time it is needed; a lane is shown to it as open documents, closed
    again after the question. It is configured in `.kito/config.toml`,
    `[semantic.servers.<name>]`; `typescript-language-server` and `gopls`
    are the defaults, and a server that is not installed is skipped.
- **A rename reaches the uses the other side has.** When a lane is
  integrated and one of the two sides renamed a symbol the other still
  calls by its old name, the providers are asked who uses it. The uses a
  provider is sure of take the new name: the new ones another lane added,
  and the old ones the author of the rename forgot. The integration result
  says so (`rename_propagation`, with the places), and the verifier runs on
  the result as always.
- **`semantic_ref` conflicts.** A use that only the syntax suspects, and
  that is new on its side, is not touched: it is a conflict, with the
  rename on one side and the list of the uses on the other. It is decided
  with `kito resolve <conflict> --rewrite` (they are uses of the symbol:
  they take the new name) or `--keep` (they are something else), or the
  same choices through `kito.resolve_conflict`.
- **`kito rename` takes the uses along**: the ones a provider vouches for
  are renamed in the lane with the definition, the others are listed as
  before.
- A language server that fails, or does not answer in time, is stopped and
  said so in the result; the syntax answers in its place.

### Demo

The scenario of the plan: alice renames `getUser` to `fetchUser`; bob, in
another lane, adds a file that calls `getUser`. With git the two merge
without a conflict and the result does not compile. With Kito and a
language server, bob's call is renamed when his lane is integrated, and the
type checker and the tests pass before anything reaches the main lane.
Without a language server Kito asks, and applies the answer.

```bash
cargo build --release -p kitod -p kito-cli
```

```bash
scripts/demo-phase4.sh
```

It needs `tsc` and `node`; the second part also needs
`typescript-language-server`. Output of the run of 14 November 2026 in
[`demo/phase4-demo.txt`](demo/phase4-demo.txt). The heart of it:

```text
$ git merge --no-edit bob
Merge made by the 'ort' strategy.

$ tsc --noEmit -p .
src/billing.ts(1,10): error TS2305: Module '"./auth"' has no exported member 'getUser'.
```

```text
$ kito rename src/auth.ts:getUser fetchUser --lane lane/1
`getUser` is now `fetchUser` in src/auth.ts (symbol 0#0).
Its uses took the new name:
  src/web.ts:1
  src/web.ts:4

$ kito integrate lane/1
lane/1: integrated as ckpt#2 (git e4cd595d93), verified in 2.4 s

$ kito integrate lane/2
lane/2: integrated as ckpt#3 (git 420722a3a6), verified in 2.4 s
  rename followed: ts/auth.getUser#0 is now `fetchUser` also in src/billing.ts:1, src/billing.ts:4
```

and, without a language server:

```text
$ kito integrate lane/2
lane/2: 1 conflict
  conflict#1   lane/2   semantic_ref   getUser  (src/auth.ts)  [to decide]

$ kito conflicts conflict#1
--- on the main lane
renamed: getUser → fetchUser

--- on lane/2
uses of `getUser` that may be of this symbol:
src/billing.ts:1: import { getUser } from "./auth";
src/billing.ts:4: return "invoice for " + getUser(id);

$ kito resolve conflict#1 --rewrite
$ kito integrate lane/2
lane/2: integrated as ckpt#3 (git 5cd39ec641), verified in 2.3 s
  rename followed: ts/auth.getUser#0 is now `fetchUser` also in src/billing.ts:1, src/billing.ts:4
```

### Acceptance criteria rechecked

**0 fake conflicts.** The swarm of phase 3, run again on this code: 40
integrations out of 40, no conflict that is not one of the 3 staged. A new
kind of fake conflict was found and removed while writing this phase: a
line one side changed and the other only renamed a symbol in. The three
states are now made to agree on the name before they are merged (D-060).

**Integration under 200 ms per 100 ops.** An integration in which nothing
was renamed asks nothing and costs what it did: 87 ms for 100 ops on 50,000
files, 425 ms for 1,000. One that has to ask pays for the answers:

| Who answers | Explicit rename in a lane | Integrating a lane that renamed |
| --- | --- | --- |
| TypeScript server, first question (it starts and loads 41 files) | 1,060 ms | — |
| TypeScript server, later questions | 224–240 ms | — |
| The syntax alone, 5,000 files | 109 ms | 122 ms |
| The syntax alone, 50,000 files | 817–893 ms | 756–773 ms |

**Latency of a save.** Unchanged: processing p50 14.9 ms, p95 19.9 ms on
the ordinary file. Level 2 is not on the path of a save.

### Tests

226 in the workspace. New in this phase:

- Names and imports read from the syntax, in TypeScript, Python and Go.
- The heuristic: same file, named and namespace imports, a namesake from
  another module left alone, methods, Python and Go packages; references
  leaving a file.
- The language-server provider against a server written for the tests:
  lanes as open documents, a server that hangs, one that dies, one that is
  not installed; and against the real `typescript-language-server` and
  `gopls`, which run where they are installed (they say when they are
  skipped, and `KITO_TEST_REQUIRE_SERVERS=1` forbids it).
- Integrations: a rename meets a new use, in a new file and on a line the
  rename touched too; a rename reaches what the main lane got meanwhile
  and what its author forgot; a doubtful use asked about, rewritten, kept;
  an explicit rename with and without a provider that is sure.

### Known limitations

- **No reference index.** The plan has level 2 fill an index in the
  background; here the providers are asked when a rename needs them, and
  nothing is stored (D-059). An index is for phase 5, with the SCIP import.
- **TypeScript and Go** are the languages tried with a language server.
  The provider is generic and takes any server from the configuration; the
  other eight languages are phase 5.
- `typescript-language-server` 6 does not work with TypeScript 7 (which has
  no `tsserver`): Kito says so and falls back to the syntax. `gopls` does
  not look into directories whose name starts with a dot.
- The syntax follows imports in JavaScript, TypeScript, Python and Go, and
  only relative module paths and names imported one by one or through a
  namespace; in the other languages it sees the same file only.
- Without a provider that is sure, the uses that were already there when a
  symbol was renamed are neither rewritten nor asked about: `kito rename`
  lists them, and they are the renamer's to fix. Only the uses another
  side added meanwhile become a `semantic_ref` conflict.
- With one, a use its author forgot is reached where its line is still
  exactly as it was; one on a line that was reformatted is left to the
  verifier.
- Only renames travel. A symbol removed on one side and newly used on the
  other, or a signature changed while a call was added, are still for the
  verifier to catch.
- The first question to a language server waits for it to load the project:
  a second on a small project, longer on a large one, within the time limit
  of the configuration (60 s by default).

## Phase 5 · coverage and performance — done

Level 2 with the language servers of eight languages; references from a
SCIP index; a store that keeps what changed instead of whole files; and the
acceptance criteria of the plan measured on a repository of 50,000 files.

### What is there

- **A finished save is captured at once.** Where the system says when a
  file is closed after being written (Linux), the daemon no longer waits
  for 50 ms of quiet: from a save to its op, 16 ms at the median and 21 ms
  at the 95th percentile on a 2,000-line file, 72 ms before.
- **Language servers for eight languages**: next to TypeScript and Go,
  JavaScript (the TypeScript server), Python (`pyright`), Rust
  (`rust-analyzer`), PHP (`intelephense`), C and C++ (`clangd`), each tried
  with the real server on the main lane and in a lane. They are the default
  servers of a repository; one that is not installed is skipped.
- **A SCIP index** (`kito scip index.scip`): the references of the whole
  repository as an indexer found them in one go, used for the files that
  have not changed since. A rename reaches the uses its author forgot
  without a language server running.
- **A store that grows with what changes.** A file saved again is stored as
  a delta from the version before: 910 bytes per save instead of 20 kB, for
  one line of a 69 kB file. The op log is untouched and every read is still
  verified against its hash.
- **Faster on large repositories**: the first capture of 50,000 files in
  14 s instead of 90, and an integration with its git commit in 0.1–0.2 s
  instead of 1.1.

### Demo

The acceptance criteria of the plan, measured one after the other on a
repository of 50,000 files, each with its verdict.

```bash
cargo build --release -p kitod -p kito-cli
```

```bash
cargo build --release -p kitod -p kito-store -p kito-diff --examples
```

```bash
scripts/fetch-corpus.sh
```

```bash
scripts/demo-phase5.sh /path/on/the/disk/to/test
```

Output of the run of 14 November 2026 in
[`demo/phase5-demo.txt`](demo/phase5-demo.txt). Its last lines:

```text
| Criterion | Target | Measured | |
| --- | --- | --- | --- |
| Save → op, p95, 2,000-line file | < 30 ms | 21.1 ms | met |
| Creating a lane, 50000 files (median of 5; best 505 ms) | < 1 s, with copy-on-write | 584 ms, files copied | met, without copy-on-write |
| Integrating without conflicts, verifier excluded, git commit included (worst of 5 lanes; 98 ms for a lane that is up to date) | < 200 ms per 100 ops | 152 ms per 100 ops | met |
| Confirmed ops lost after a crash (24 kills) | 0 | 0 | met |
| Fake conflicts (40 integrations by 10 agents) | 0 | 0 | met |
| Renames / moves kept on the corpus (ten repositories) | >= 99% | 100.0% / 100.0% | met |
```

### The acceptance criteria, one by one

**From a save to its op, under 30 ms at the 95th percentile.** Met on the
file the criterion names, 2,000 lines of ordinary code (143 functions): p95
21.1–22.6 ms in three runs of the demo. On Linux only: elsewhere the system
does not say when a file is closed, and the 50 ms of quiet remain. Not met
on the worst case kept as a reference since phase 2, 2,000 functions of one
line each: 53 ms, which is the time to read its symbols again.

**Creating a lane under 1 s on 50,000 files, with copy-on-write.** The
criterion is about a file system that shares data between files; none was
available, and every file is copied. With copies the demo measured a median
of 459 ms, 2,162 ms and 584 ms in three runs of the same binary on the same
day: the second while the disk of the test machine was in one of its slow
spells, during which creating a file costs up to twenty times more. So:
met in two runs out of three, without the feature the criterion assumes,
and not shown for the case it describes.

**Integrating without conflicts under 200 ms per 100 ops.** Met with the
git commit of the integration included: 93–110 ms for a lane that is up to
date, 135–187 ms for one the main lane has left 600 files behind, 38–49 ms
per 100 ops for a lane of 1,000. The first run of the demo measured 260 ms
for the lane left behind; what was done about it is in
[`benchmarks.md`](benchmarks.md).

**No confirmed op lost after a crash.** 0, with 21 to 24 `kill -9` per run
on 200,000 ops (and one million ops in the phase 0 demo).

**No fake conflicts.** 0 in 40 integrations by ten agents, in every run.

**Renames and moves recognized, at least 99% on the corpus.** 100% of 4,023
renames and 1,510 moves.

### Tests

239 in the workspace. New in this phase: a finished save captured without
waiting, also when it is saved through a temporary file, and a rename still
recorded as one; the real servers of Python, Rust, PHP, C, C++ and
JavaScript, on the main lane and in a lane; a SCIP index made by
`scip-typescript`, the files that changed since, a rename carried by it;
deltas in the store (size, every version read back, verification, a damaged
base, a store from before); the commit of an export made from memory equal
to the one made from the log; many new files at once equal to one by one.

### Known limitations

- **Java and C# were not tried with a language server**: none could be
  installed on the test machine. The provider takes any server from the
  configuration; examples are in the configuration file, commented out.
- **Copy-on-write was never measured**, as said above.
- **The log itself is not compacted.** The plan lists compaction between
  checkpoints; what makes the store grow is the contents, and those now
  cost what changes (D-069). The ops are all kept: about 130 bytes each.
- The first capture of 50,000 files takes 14 s where `git add` takes 2.6:
  Kito parses every file. `kito import` of a git repository of that size
  takes 30 s.
- A SCIP index is read whole into memory the first time it is needed, and
  only renames use it.
- A C or C++ project needs a `compile_commands.json` for `clangd` to find
  the uses in other files.
- Windows is still served through WSL only.

## After phase 5

- **0.5.1 · ready for Claude Code from `kito init`** (D-072). `init` and
  `import` write what Claude Code needs to work through Kito: the server in
  `.mcp.json`, the permission in `.claude/settings.json`, the rules in
  `CLAUDE.md`, `.kito/` in `.gitignore`; `kito claude` does it later.
  Tried with Claude Code itself on a project that had only `kito init` run
  in it: asked in plain words to add a function, and to rename one, it
  opened a lane, worked in it, integrated and closed.
