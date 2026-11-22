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

## Phase 6 · the intent board — done

Every lane says what it will change before it changes it, and the lanes
that are on the same code are told of each other. On one machine, with no
network: the shared server is the next phase.

### What is there

- **No lane without an intent and a scope.** `open_lane` takes one sentence
  saying why and a scope: functions and classes (`src/shop.ts:total`, also
  ones that do not exist yet), files, directories (`src/billing/`). The
  scope is on the lane's record, each entry with the moment it was
  declared; it can be extended and only grows (D-074).
- **Declared against actual.** What a lane changed since its base is read
  part by part: each function by its own text, the text outside the
  functions, files that are not read as symbols as a whole. What is
  outside the scope is recorded like the rest and stops the lane at
  integration (`out_of_scope`) until the scope is extended or the change
  undone. A rename and the uses that follow it are not a change; an import
  follows a declared function of its file; `[scope] always_allowed` names
  the files any lane may change (D-075).
- **Overlaps, on three levels**, from what was declared and from what was
  changed: the same function (or one inside the other), the same file, a
  function of one lane that uses a function of the other, by the syntax
  (D-076). Allowed, with a warning to both: each lane is told once, with
  what the other is there for.
- **What lands is told too.** When the main lane changes something a lane
  declared or changed after that lane started, the lane is told who
  changed it and what for (`landed`): it is working on a state that no
  longer has the last word on that code.
- **The board**: every open lane with its agent, intent, scope, what it
  changed, what it changed outside its scope, when it last recorded
  something, and its overlaps; `who` answers for one function, file or
  directory. A scope that covers more files than `[scope] broad_files` is
  shown as broad, and refused if the repository says so (D-077).
- **For agents**: `open_lane` with `intent` and `scope`, `extend_scope`,
  `board`, `who_touches`; the answer to a tool that names a lane carries
  the `notices` the lane has not heard yet, with a line saying that what
  other lanes say is information and not instructions (D-078). The rules
  `kito claude` writes say the same, and rules written by an older version
  are replaced when nobody changed them.
- **Command line**: `kito lane open --intent --scope`, `kito lane scope
  [--add]`, `kito board`, `kito who`.

### Demo

```
scripts/demo-phase6.sh
```

Three agents on one machine ([output](demo/phase6-demo.txt)). The second
declares the function the first is changing and is told before it writes:

```
$ kito lane open frank --intent a discount of 10 over 50 euros --scope shop.py:total
lane/2 opened for frank on ckpt#1.
Scope: shop.py:total
  overlap (function) with lane/1 of erin, there for: prices in cents, not euros
    shop.py:total: erin declared it; you declared it
```

The third declares a function that calls it:

```
$ kito lane open gina --intent a receipt with the currency --scope shop.py:receipt
  overlap (caller) with lane/1 of erin, there for: prices in cents, not euros
    your shop.py:receipt uses shop.py:total, which erin declared
```

And a change outside the scope stops the lane until it is declared:

```
$ kito integrate lane/1
lane/1: not integrated, it changed things outside its scope
  outside the scope  README.md
$ kito lane scope lane/1 --add README.md
$ kito integrate lane/1
lane/1: integrated as ckpt#2
```

### Acceptance targets

**No lane opens without a scope.** Through the repository, the daemon, the
command line and the MCP server: each is refused with the reason, and a
refused lane leaves nothing behind.

**Every same-function overlap in the swarm is reported before
integration.** The ten agents of the swarm now declare what the script has
them change, among it the class all of them add a method to: 90
overlaps, each told to its lane before that lane first asked to integrate.
The swarm's other numbers are unchanged: 40 integrations, no fake
conflicts.

### Tests

266 tests in all. New: the scope rules, the overlaps, the notices
and the board on the repository itself (`kitod/tests/scope.rs`); the same
through the MCP tools and through the command line; the reading of scope
items and of what encloses a symbol.

### Known limitations

- **Callers are found by the syntax only**: in the same file and through
  explicit imports (JavaScript, TypeScript, Python, Go), with no language
  server asked. A use the syntax does not resolve is not an overlap.
- **A declared scope is checked, not enforced where the agent writes.** A
  write outside it is found at the next question and at integration. A
  hook that refuses the edit in Claude Code is not written.
- **Notices reach an agent with the answer to its next tool.** An agent
  that asks nothing hears nothing; a person sees them with `kito board`.
- **What was told is kept in memory**: after a restart of the daemon each
  lane hears its overlaps once more.
- **Lanes opened by an older version have no scope**: they are not
  integrated until one is declared.
- The overlaps of every pair of lanes are computed when asked: nothing
  above ten lanes has been measured.

## Phase 7 · one server, several repositories — done

A server that keeps several repositories, clients that reach it from
directories of their own, and what happens in a repository told to whoever
watches it. The working copy of a lane is still on the server: editing it
from another machine is phase 8.

### What is there

- **`kito-server`**, a door in front of ordinary repositories (D-079). Each
  repository under the server's directory has its own daemon, started when
  first asked for; the server knows who a client is, which repositories
  that person may use, and passes the requests of a session on to the
  daemon of the repository the session is about. `init`, `repo add` (empty,
  or with the history of a git repository), `user add | list | remove`,
  `serve`, `stdio`.
- **Every session is authenticated** (D-080). Over TCP with a token, shown
  once when the person is added and kept only as what it hashes to; through
  a command (`stdio`), which is how SSH carries it, with the person SSH
  established. No token, a wrong one, a repository that is not there and
  one that is not theirs are refused, the last two in the same words.
- **What a client may ask** is a list (D-081): seeing lanes and their
  history, opening and closing them, scope, intents, integrations,
  conflicts. Stopping a daemon, checkpoints and rollbacks of the main lane
  are not on it.
- **Whose lane it is.** A lane records the person it was opened for: the
  one the server authenticated, whatever the client sent; on one machine,
  whoever runs the repository. The board and the overlaps say whose agent
  each lane is.
- **Events** (D-082): the daemon numbers what happens (lanes opened and
  closed, scope, intents, saves wherever they were recorded, integrations
  and how they end, conflicts decided) and a request waits for what comes
  after a number. `kito watch` prints them as they come.
- **Connected directories** (D-083): `kito connect <server> <repository>`
  makes a directory a client of a repository, with nothing in it but how
  the server is reached. `kito`, and `kito-mcp` for agents, work there as
  they do in a repository; `kito repos` lists what the person may use.

### Demo

```
scripts/demo-phase7.sh
```

One server, two repositories, two people ([output](demo/phase7-demo.txt)).
Ada may use both repositories, bob only the shop. Bob watches while ada
opens a lane:

```
(bob) 10:40:01  lane/1    lane opened  by ada  builder: prices in cents [src/shop.py:total]
Bob saw it 13 ms after ada asked.
```

His board says whose lane it is, and what is not his is refused:

```
$ kito board
lane/1  builder (of ada)  active  prices in cents
$ (bob) kito connect 127.0.0.1:7421 site bob-site --token kito_…
kito: the server refused the connection: there is no repository `site` for bob on this server
$ kito checkpoint -m from afar
kito: `checkpoint` is not something a client can ask through the server
```

### Acceptance targets

**A lane opened on one client shows on the other within 1 s.** In the test
the second client is waiting on the events when the first opens the lane:
it has the event well inside the second (the test fails above 1 s), and the
demo measured 13 ms on one machine. Two machines were not available: the
network between them is not in that number.

**Every request is authenticated.** A session answers nothing before its
greeting, and the greeting is refused without a token the server knows.
Tested over TCP and through a command; SSH itself was not available to
try.

**Several repositories.** Two in the tests and in the demo: each with its
own lanes, events and daemon, and a person limited to one cannot name the
other.

### Tests

273 tests in all. New: the server with its real daemons
(`kito-server/tests/server.rs`: people and tokens, authentication and
access, a lane seen by another client, the protocol through a command); a
directory connected to a server from the command line; the MCP tools from
a connected directory.

### Known limitations

- **Nothing is encrypted by the server.** `serve` listens on this machine
  only by default; across machines the transport is SSH, which could not
  be tried here. (Encrypted since phase 12.)
- **A lane's working copy is on the server.** A client on another machine
  sees lanes and decides about them, and cannot yet edit their files.
- **One role.** Whoever may use a repository may do everything a client
  can: who may integrate is phase 9.
- **Tokens do not expire**, and removing a person is the only way to take
  one back.
- **A repository with no files cannot have lanes**: `repo add` without
  `--from` makes one that is filled on the server.
- One thread for each client, and one connection to a daemon for each
  session: nothing above a handful of clients has been measured.

## Phase 8 · lanes from any machine — done

A lane is still kept by the server; its working copy can now be on any
machine connected to the repository. One machine writes in a lane, any
number follow it, and a daemon on each keeps its copy in step with the
server.

### What is there

- **A daemon for a connected directory** (D-084). `kitod` started in a
  connected directory answers the same requests as the daemon of a
  repository, by asking the server, and keeps on this machine the working
  copies of the lanes worked on or followed from here. The command line
  and the MCP server did not have to change: they find a socket, as in a
  repository.
- **The directory is the project** (D-085). `kito connect` brings the files
  of the main lane into the directory and keeps them as the server has
  them, so that a person or an agent can read the code before opening a
  lane. Changes are made in lanes: what is edited in the directory itself
  is neither sent nor overwritten.
- **A lane opened from here is edited here** (D-086). `open_lane` answers
  with a working copy on this machine. Each save found there is sent to
  the server with the content it started from, and recorded in the lane as
  the agent's, for the person the server authenticated. A lane that moved
  on in the meantime (an integration, a rename made on the server) is
  merged with the save; when the two do not merge nothing is changed and
  the file is marked: `kito lane take <lane> <file> --mine | --theirs`
  settles it.
- **A journal of saves** (D-087). A save is written to a journal on disk
  before it is sent, and leaves it when the server has answered. A daemon
  killed at any moment finds the journal and the files, and carries on; a
  save sent twice is one save. With the server away the saves wait, and
  `kito daemon status` says how many.
- **Followers** (D-088). `kito lane attach <lane>` brings the files of
  anybody's lane to this machine and keeps them in step: the events of the
  server now carry the paths that changed, and a follower fetches those. A
  file changed in a follower's copy is kept, and shown as not sent.
- **One machine writes in a lane at a time** (D-089), and only for the
  lane's owner. `kito lane attach <lane> --write` on another machine takes
  the lane over: the first machine follows from then on, and what is saved
  on it waits, with the reason, until it takes the lane back.
- **The barrier** (D-090). Before `sync`, an integration request or a
  rename, everything saved here is sent; if something did not get there
  (the server is away, a file does not merge) the request is refused with
  the reason, instead of integrating a lane that lacks the last edits.
- On the way: the server learns of people added while it runs; a daemon is
  asked whether it is there without computing its whole status; a client
  daemon keeps its connections to the server for the next command; `kito
  connect` sets the directory up for Claude Code as `kito init` does.

### Demo

```
scripts/demo-phase8.sh
```

An agent on one machine, a person following on another
([output](demo/phase8-demo.txt)). Ada connects a directory and opens a lane
for her agent; bob connects another and follows it:

```
$ kito lane open agent-a --intent prices in cents --scope src/ README.md
lane/1 opened for agent-a on ckpt#1.
Working copy: /tmp/kito-demo/laptop/.kito/lanes/1/work

$ (bob) kito lane attach 1
lane/1 is here: /tmp/kito-demo/desk/.kito/lanes/1/work
It follows the lane: its files change as the lane's do. What is saved there stays there.

milliseconds from the save to the follower's disk, 20 saves: median 18, slowest 72
```

Ada's daemon is killed while the agent works, and the server goes away
for a while:

```
$ kill -9 <pid>    # the daemon of the laptop
(two more saves, with no daemon to see them)
$ kito sync 1
The lane on the server has exactly what is on the laptop.

$ kill <pid>    # the server
$ kito sync 1
kito: 1 save made here did not reach the server yet: cannot reach the server: 127.0.0.1:7421: Connection refused (os error 111)
$ kito-server --dir /tmp/kito-demo/server serve --listen 127.0.0.1:7421 &
$ kito sync 1
The lane on the server has exactly what is on the laptop.
So has bob, at his desk.
```

### Acceptance targets

**A save is on a follower's disk within 300 ms at the 95th percentile.**
A hundred saves in a writer's copy, each timed from the moment the file is
on the writer's disk to the moment the same content is on the follower's:
median 19 ms, 95th percentile 21 ms, slowest 80 ms (the test fails above
300 ms). Both machines were one machine: the network between two is not in
that number. A save crosses it about four times (the save, the event, the
list of what changed, the content), so a network with a 20 ms round trip
would add about 80 ms.

**No acknowledged save is lost when the client is killed.** The test kills
the daemon twelve times at moments it does not choose (before it saw a
save, while sending, with a journal half sent), keeps saving while no
daemon runs, and after each restart and barrier compares the lane on the
server with the files: they are the same every time, and the journal ends
empty. The same with the server away. The five tests of this phase passed
25 runs in a row.

**An agent on one machine, a person following on another**: the demo
above, and a real agent below.

### A real agent

Claude Code 2.1.326, started in a directory that had only `kito connect`
run for it, was asked in plain words for a change to one function. It read
the file in the directory, opened a lane with its intent and scope, edited
the working copy on its own machine, asked for the integration, read the
result and closed the lane: eight turns. The change was on the server's
main lane as a git commit, and in the other person's connected directory,
without anybody copying a file.

### What it costs

On one machine, with a repository of 2,000 files (17 MB) and one of 20,000
(87 MB): opening a lane with its files brought here takes 1.3 s and 3.2 s;
the barrier with nothing to send, 80 ms and 190 ms; any other command in
a connected directory, about 10 ms and 50 ms.

### Tests

286 tests in all. New: the requests for a working copy elsewhere
(`kitod/tests/files.rs`: reading a lane, a save recorded as the lane's own,
a save on an older version merged or refused, removals, what is refused);
the real server with the real daemons of connected directories
(`kito-server/tests/mirror.rs`: a lane edited from another machine, the
follower's 300 ms, the kills, a lane that moved on, one writer at a time);
the new commands from the command line; an agent's whole piece of work
through the MCP tools from a connected directory.

### Known limitations

- **Two real machines were not available.** Everything was measured and
  tried over the loopback interface; SSH as the transport is still
  untried, and the server still encrypts nothing (it does since phase
  12).
- **Opening a lane brings all its files**, each time: copies on one
  machine do not share contents, and whole files travel (no differences,
  one message for each file). Large binary files are costly.
- **Symbolic links made in a writer's copy are not sent**; `kito daemon
  status` says so.
- **Only the lane's owner writes in it**: two people cannot both write in
  one lane. Which machine writes is kept in the server's memory: after a
  restart it is the first that sends a save.
- **A file changed in a copy at the very moment the server's version of it
  is being written there** can be overwritten: the check and the write are
  microseconds apart, not one step.
- **If a repository's daemon restarts while the server keeps running**, a
  follower can take up to 20 s to notice that it missed events; then it
  compares everything.
- The look at a working copy before the barrier is proportional to the
  number of files (190 ms at 20,000), and so is the periodic one.
- Verification commands still run on the server.

## Phase 9 · protected main and team rules — done

The main lane changes only when a lane is integrated, the checks every
lane must pass run where the repository is, people have roles, and each
integration goes to a git remote.

### What is there

- **A protected main lane** (D-092). With `kito protect`, or from the start
  for a repository a server added with its history, nothing saved in the
  main directory is recorded: the file is put back as the main lane has
  it, the edit is kept under `.kito/refused/<when>/`, and whoever watches
  is told. A checkpoint by hand, a rollback and a rename made directly on
  the main lane say no. An integration is the one way in; `kito protect
  --off` opens the main lane again, and says so in the events.
- **Roles** (D-094): `read` sees the lanes, the board and the history and
  follows lanes; `write` also opens lanes and works in them; `integrate`
  also brings lanes into the main lane. One role for each person, which
  can differ in one repository: `kito-server user add bob --role write`,
  `kito-server user role bob integrate --repo shop`.
- **A lane is its owner's** (D-098): closing it, extending its scope,
  changing its intent, renaming in it, asking for its integration and
  deciding its conflicts are for the person it was opened for, or for
  somebody who may integrate.
- **Waiting for approval** (D-095). A lane whose owner may not integrate
  is still merged with the main lane and checked when they ask; if that
  goes well it is marked `awaiting_approval`, with who asked, and nothing
  is written. Somebody who may integrate brings it in with `kito integrate
  <lane>`: the merge and the checks run again on the main lane as it is
  then. A lane that changes after it was checked has to ask again.
- **The checks are the repository's** (D-096): `kito-server repo set shop
  --check "npm test"` on the server; no client can change them, and a
  failing one stops a lane whoever asks.
- **Every integration is pushed** (D-097) to the git remote the repository
  names (`repo set shop --push <remote>`): never forced, after the lane
  has its answer; a push that fails is said in the events and in `kito
  status`, and goes with the next one.
- For agents: the rules `kito init`, `kito connect` and `kito claude`
  write now cover `awaiting_approval` (leave the lane open, say that it
  waits); rules written by 0.6 to 0.8 are brought up to date if nobody
  edited them.

### Demo

```
scripts/demo-phase9.sh
```

A server, a repository whose test is its check, three people
([output](demo/phase9-demo.txt)). **A direct edit to main is refused:**

```
$ echo "def total(price, quantity): return 0" > /tmp/kito-demo/server/repos/shop/src/shop.py    # on the server, by hand
$ cat /tmp/kito-demo/server/repos/shop/src/shop.py
def total(price, quantity):
    return price * quantity
(carol) $ kito watch --past
10:40:08  lane/main  main edit refused  src/shop.py put back as the main lane has it; the edit is kept in …/.kito/refused/2026-11-17T10-40-08.513Z
```

**A failing test stops a lane**, and one that passes waits for somebody
who may integrate:

```
(bob) $ kito integrate 1
lane/1: verification failed: `python3 tests/test_shop.py`
  | AssertionError: total(2, 3) should be 6, it is 600
  Nothing was integrated: fix it in the lane and integrate again.

(bob) $ kito integrate 1
lane/1: merged and checked, verified in 0.0 s; it waits for somebody who may integrate

(ada) $ kito board
lane/1  agent-b (of bob)  awaiting approval (asked by bob)  prices in cents

(ada) $ kito integrate 1
lane/1: integrated as ckpt#2 (git 7ee4b1be89), verified in 0.0 s

$ git -C /tmp/kito-demo/upstream.git log --format=%h %an: %s main
66bf095 agent-b: prices in cents
635d14b demo: start
```

### Acceptance target

**No change reaches main without the merge and the checks.** On a
protected main lane the ways in were each tried and each refused or
undone: a file changed, created or removed in the main directory (put
back; 5 ms at the median over 50 edits, 7 ms the slowest); a checkpoint by
hand; a rollback; a rename made on the main lane; an integration asked by
somebody who may not integrate (held); a lane that fails a check (stopped,
for whoever asks); a client changing the checks (not a request a client
can make). What does change it: an integration by somebody who may, which
merges and checks; and whoever runs the repository turning the protection
off, which is recorded.

### A real agent

Claude Code 2.1.326, working for a person who may write and not integrate,
on a repository whose check is its test: it declared the function and the
test, changed both in its lane, asked for the integration, read
`awaiting_approval`, left the lane open and reported that it waits for
somebody who may integrate, with the command. Brought in afterwards by the
person who may, the commit carries the agent's name.

### Tests

294 tests in all. New: the protected main lane (`kitod/tests/protect.rs`:
edits put back and kept, a lane as the way in, what is refused and how the
protection is lifted); the team's rules against the real server
(`kito-server/tests/rules.rs`: the three roles and approval, checks on the
server, the main lane on the server, the push to a git remote, one that
moved on by itself included); `kito protect` and the wait for approval
from the command line; the rules for agents brought up to date.

### Known limitations

- **The protection undoes, it does not prevent.** A file edited in the
  main directory is that way for a few milliseconds, and for as long as no
  daemon runs; a program that reads it then sees the edit.
- **It guards against mistakes and agents, not against whoever runs the
  server**: with a shell there the protection can be turned off, and
  `.kito/` edited.
- **The checks run on the server as the server's user**, on code a lane
  wrote, with no sandbox. (They are confined since phase 12.)
- **Approval is one person integrating.** No comments on a lane, no number
  of approvals, and the owner learns that their lane went in from the
  board and the events.
- **The push needs `git` on the server and credentials that work with
  nobody there.** A remote that got commits of its own is not merged:
  pushes fail, and say so, until somebody reconciles the two by hand.
- **Roles are a person's.** An agent has the role of the person it works
  for; a repository on one machine, with no server, has no roles.
- `.kito/refused/` only grows.
- Two real machines and SSH are still untried.

## Phase 10 · the why graph — done

Not in the first plan: asked for after phase 9, so that the agents of a
team can find out why code is the way it is before they change it. (The
plan's phase 10, agents started from the server, moved on as phases were
added: it is the last of the plan.)

Kito already records a reason with everything: the intent a lane declares,
the lane, the agent, the person it works for, the integration, the checks.
This phase turns that into something to ask.

### What is there

- **`why` on a function, a file or lines** (D-101). `kito why
  src/shop.ts:total`, `kito why README.md`, `kito why src/shop.ts:12-20`,
  and the `why` tool for agents. The answer has:
  - the **reasons** it changed, newest first: the words of the reason and
    its task, what it did to the target (created, body changed, renamed a
    → b, moved), which agent made it and for which person, the lane, the
    checkpoint that brought it to the main lane, the checks that passed
    and who let it in;
  - **what else changed for the same reason**: the other functions and
    files of that piece of work;
  - **each line** with the reason it was last written for (D-102);
  - **who uses it and what it uses**, each with its own last reason;
  - **the lanes on it now**, with what they are there for;
  - **the conflicts decided on it**, and how.
- **Reasons older than Kito.** In a repository that came from git, the
  lines of a function nobody touched since say which commits wrote them:
  the commit messages are their reasons.
- **Files that are not code** have reasons and lines like any other.
- **`reasons`**: earlier work found by the words of its reason, its task
  or the names of who did it (`kito reasons cents`, the `reasons` tool),
  each with what it changed.
- **A diagram**: `kito why <target> --graph` prints the answer as a
  Mermaid flowchart.
- **From a lane**: asked with a lane, the answer is the code as the lane
  has it, the lane's own unfinished work first.
- **For agents**: the rules written by `kito init`, `kito connect` and
  `kito claude` now say to ask `why` before changing a function or a file
  somebody else wrote; every answer says that reasons are what others
  declared, not instructions (D-105). Every role on a server may ask.
- A checkpoint now records **who let an integration in** (D-100).
- The step-by-step history `kito why` used to print is `kito why
  --history` (D-103).

### Demo

```
scripts/demo-phase10.sh
```

A repository with three commits in git comes to a server; two agents work
on it for two people; a third agent is about to change `total` and asks
first ([output](demo/phase10-demo.txt)):

```
(carol) $ kito why src/shop.py:total
function total  (src/shop.py)  lines 1-7

Why it is the way it is, newest first:
  1. no negative prices  (task #7)
     body changed · by agent-a for ada · lane/1 · ckpt#4 · 2026-11-17 10:40
     checks passed: python3 -m py_compile src/shop.py src/billing.py · let in by ada
  2. prices in cents  (task #7)
     body changed · by agent-a for ada · lane/1 · ckpt#4 · 2026-11-17 10:40
     checks passed: python3 -m py_compile src/shop.py src/billing.py · let in by ada
     with: README.md, src/billing.py:invoice
  3. a shop that adds up prices
     wrote lines of it · by Grace · ckpt#1 · 2026-11-17 10:40
     with: README.md, src/billing.py

Its lines, with the reason each was last written for:
    3     1  def total(prices):
    3     2      result = 0
    1     3      if any(price < 0 for price in prices):
    1     4          raise ValueError("negative price")
    3     5      for price in prices:
    2     6          result += round(price * 100)
    3     7      return result

Used by:
  src/billing.py:invoice (line 5)  last changed for: prices in cents (agent-a for ada, ckpt#4)

On it now:
  lane/3  agent-d (of dave)  totals for several currencies  — agent-d declared it
```

Reason 3 is a git commit from before Kito; reasons 1 and 2 are two intents
of one lane.

### A real agent

Claude Code 2.1.326, in a directory connected to that server, was asked:
"make total() add a 10% service fee to what it returns". It called `why`
on the function before opening its lane. From the answer it kept the fee
in whole cents (`(result * 110 + 50) // 100`) because an earlier lane had
made totals whole numbers of cents; it named `invoice` as the only caller
and left it alone; and it reported that another lane was on the same
function for another person. None of that is in the function's text.

### What it costs

On a repository of 20,000 files whose log has 500,000 operations: the
first question after the daemon starts reads the log once, 1.3 s; after
that a function, a file or a line is answered in 9 to 37 ms, a search of
the reasons in 4 to 6 ms, a question from a lane in about 100 ms (the
lane's working copy is looked at first). Looking for who uses a function
is skipped above 5,000 files unless asked: asked, it adds 0.3 s. The index
adds about 20 MB to the daemon.

### Tests

304 tests in all. New: the why graph on a team's history
(`kitod/tests/why.rs`: reasons and lines of a function, lines and files,
work in progress and decisions and the view from a lane, the search, code
older than Kito with the reasons git recorded, hand edits that take the
reason of their checkpoint, the index built once); `kito why`, `--graph`
and `kito reasons` from the command line; the `why` and `reasons` tools;
through a server, for somebody who may only read, with who let a lane in.

### Known limitations

- **A reason is as good as what was declared.** "fix" tells nothing, and
  nothing checks the words.
- **Who uses what comes from the syntax**: the same file and what is
  imported by name. A language server is not asked.
- **A file's lines are not followed across a rename of the file**, nor a
  function's across a move to another file: they count as written where
  they are now.
- **At most 400 versions of a file are read** for its lines; what is older
  is told as one.
- **The index lives in memory**: the first question after a restart reads
  the whole log.
- **Nothing can be said about code without changing it**: there are no
  notes attached to a function.
- **Who let a lane in** is known for integrations made from this version
  on.
- The prompt an intent may carry is not shown.

## Phase 11 · the rules as behaviour — done

Not in the first plan either: asked for right after phase 10. The why
graph was there to be asked, and the rules told agents to ask; whether
they did was up to each model. Now it is not.

Three rules hold for every agent: work in a lane, change what the lane
declared, know why code is the way it is before changing it. Each is
checked at the moment it matters, by something that runs whatever the
model decided.

### What is there

- **The why comes with the scope** (`kitod/src/guard.rs`). The answers of
  `open_lane` and `extend_scope` carry `why`: for each function and file
  declared, its last five reasons (the words, the task, who, for whom,
  the lane, the checkpoint) and up to eight of its users. Declaring is
  mandatory, so every agent on every model has the reasons before its
  first edit. A directory is not told file by file: a note says so.
  `kito lane open` and `kito lane scope --add` print the same.
- **What a lane was told is remembered** with the lane
  (`.kito/lanes/<n>/told.json`): by a declaration, by a held edit, by a
  held integration, or because its agent asked `why` for its lane. Nothing
  is told twice, a restart of the daemon included.
- **`before_edit`**, a request to the daemon: may this edit be made? Not
  in the main directory (unless Kito ignores the file); not in a lane that
  is not open; not outside what the lane declared, down to the function
  the lines are in; and, on lines written for reasons the lane was not
  told, not until it was: the edit is refused once with the reasons, which
  are then counted as told. The lines are found through the history of the
  file, as `kito why file:line` finds them; the lane's own work is no news
  to it.
- **`reasons_behind`**: the reasons behind the lines a lane replaced or
  removed since it started, that it was not told. `kito-mcp` asks it
  before `request_integration`: if there are any, the tool answers
  `read_first` with them and starts nothing; asked again, the integration
  goes ahead. This is the net under everything else: a change made through
  a shell, or by an agent that has no hooks.
- **`after_shell`**: what changed since a command started where it should
  not have: files of the main directory written by hand (put back, on a
  protected main lane, or recorded), and lanes with changes outside their
  scope.
- **Hooks for Claude Code** (`kito hook`, `kito-cli/src/hook.rs`), written
  into `.claude/settings.json` by `kito init`, `import` and `claude`:
  `before-edit` before `Edit|Write|MultiEdit|NotebookEdit` (it finds the
  lines an edit replaces, asks `before_edit`, and answers `deny` with the
  words for the model); `after-shell` after `Bash`; `session-start` (the
  three rules, and the lanes open now). The settings of a project that
  already has hooks keep them; a second run changes nothing.
  `kito hook before-edit --file F --lines A-B` is the same question for
  any other tool: words, and exit code 2.
- **From a connected directory** the daemon passes the questions on to the
  server, after sending it what was saved (the file is judged as it is
  here). The directory's own files follow the main lane: what a command
  wrote in them is told by the local daemon. Somebody who may only read
  may ask `before_edit`.
- **`[agents] why = false`** in a repository's configuration: the reasons
  are told at the declaration, and nothing is held.
- **Events**: `edit_refused`, for an edit in the main directory or outside
  the scope, for whoever watches (`kito watch`).
- The rules in `CLAUDE.md` say what is enforced (older rules are brought
  up to date), and the instructions of the MCP server too.

### Demo

```bash
cargo build --release -p kitod -p kito-cli -p kito-mcp
```

```bash
scripts/demo-phase11.sh
```

The demo runs the hooks by hand, with what Claude Code says to them: no
agent is needed. Output: [`docs/demo/phase11-demo.txt`](demo/phase11-demo.txt).
An excerpt:

```text
(agent) Edit .kito/lanes/3/work/src/shop.py:          result += round(price * 100)  ->          result += int(price * 100)
  [NOT MADE]
  Kito: src/shop.py:total is outside what lane/3 declared (src/shop.py:discount), so this edit
  was not made. Declare it first: call the extend_scope tool with lane "lane/3" and scope
  ["src/shop.py:total"]. Its answer tells why that code is the way it is and who else is on it.
  Then make this edit again.

$ kito lane scope 3 --add src/shop.py:total
Why it is the way it is:
  src/shop.py:total
    "prices in cents" (task #7): agent-a for ada, lane/1, checkpoint ckpt#2
    "a shop that adds up prices": ada, checkpoint ckpt#1
    used by src/billing.py:invoice (line 5)
```

### A real agent

Claude Code 2.1.326 (`claude -p`), four runs on a small repository with a
history made through lanes.

1. **No rules in `CLAUDE.md`, a plain request** ("change discount so that
   the cut is rounded up; make invoice say 'total due: '"). The session
   started with what the `session-start` hook told it. It opened a lane
   declaring the two functions, read the reasons in the answer ("whole
   cents off, never a fraction": rounding up still takes whole cents off,
   it said in its report), edited, and integrated. Its test run left
   `__pycache__` files in the lane: the `after-shell` hook told it they
   were outside its scope, and it removed them before integrating.
2. **No rules and no `session-start` hook.** The same: the instructions of
   the MCP server were enough for it to open a lane first.
3. **Asked to break the rules on purpose**, as a test: edit `total`
   without declaring it, and edit `README.md` in the main directory. Both
   edits were refused by the hook and not made; it called `extend_scope`
   for each, was told why `total` is the way it is and who uses it, made
   the edits in the lane, and integrated.
4. **A lane on a whole directory, an edit and a `sed`.** Its `Edit` of
   `invoice` was held back once with "prices in cents" (task #7); it made
   the same edit again. Its `sed` on `discount` ran unchecked;
   `request_integration` answered `read_first` with "whole cents off, never
   a fraction"; it asked again and the lane was integrated. It quoted both
   in its report.

A real agent that knows the rules follows them: runs 1 and 2 never met a
refusal. The hooks are for the times it does not.

### What it costs

On the repository of 20,000 files and 500,000 operations of phase 10:
`kito hook before-edit` on a file of a lane takes 21 to 25 ms, held or
not, and 3 ms for a file of the main directory; `after-shell` about 90 ms,
60 of which it waits for the save to be recorded; `session-start` 100 to
130 ms. Opening a lane on one function, with its reasons, takes 0.4 s
(0.7 s for a directory, which lists more files); the first question after
the daemon starts still reads the log once (1.3 s).

### Tests

317 tests in all. New: the guard on a team's history
(`kitod/tests/guard.rs`: the why of what is declared, the main directory,
the scope down to the function, an edit held once and not again after a
restart, a lane that asked why, the reasons behind what was replaced
before an integration, what a command changed); the hooks from the command
line, fed what Claude Code sends them; the settings written and kept
(`claude.rs`); the tools (`why` on declaring, `read_first`); and from a
connected directory, through a server.

### Known limitations

- **A shell command is checked after it ran**, not before: what it wrote
  in a lane outside the scope is told and stops the integration; what it
  replaced is told before the integration. Nothing is undone.
- **Hooks are Claude Code's.** Other agents get the reasons when they
  declare and before they integrate, and may call `kito hook before-edit
  --file`; nothing makes them.
- **Only the agent is held, and only through the tools.** `kito integrate`
  from a shell holds nothing: it is the command of people. An agent that
  runs it skips the reasons, not the scope.
- **A hook that cannot reach the daemon lets the edit through.**
- **An agent can edit the hooks out of `.claude/settings.json`**, in a
  lane, like any other file: it shows in the lane and in what is
  integrated, and is not prevented.
- **Being told is not agreeing.** An edit goes through the second time
  whatever the reasons say. Whether a change that undoes a reason should
  land is for whoever integrates.
- **A new function in a file where another was declared** is found
  outside the scope when the lane is integrated, not at the edit.
- **In a connected directory, a new file written in the directory itself**
  is not reported after a command (a changed one is).
- **Reasons are as good as what was declared**, as before.

## Phase 12 · encrypted connections and a sandbox per lane — done

The first of five phases asked for after phase 11, to make a shared
server safe: until now it encrypted nothing, and the checks of an
integration ran code written in a lane as the server's own user. (Of the
plan's "agents started from the server", the sandbox is here; the runner
is still to come.)

### What is there

- **TLS between clients and `kito-server`** (`kitod/src/tls.rs`,
  `kito-server/src/tls.rs`; rustls with ring). The server makes a key and
  a self-signed certificate the first time it needs them, in `tls/`.
  Clients know the server by the SHA-256 fingerprint of the certificate:
  `kito-server fingerprint` prints it, `user add` prints the `kito connect`
  line with it, and `serve` says it when it starts.
- **`kito connect --fingerprint`**: kept in `.kito/remote.toml`; a server
  that presents another certificate is refused before the token is sent.
  A server on this machine is taken at its word the first time and held to
  it afterwards. `--authority` for a certificate from a public authority
  (the server is known by its name); a server on another machine with
  neither is refused, and the error shows the fingerprint it presents.
- **`serve --plain` / `connect --plain`**: no encryption, loopback only.
  `serve` may now listen on any address, since what it carries is
  encrypted.
- **The sandbox** (`kitod/src/sandbox.rs`): Landlock, applied by `kitod
  sandbox <policy> -- <command>`, which gives up rights and then becomes
  the command. A confined command writes only where its policy says, does
  not read what is hidden from it, and (without network) makes and accepts
  no TCP connection. "Everything but these directories" is spelled as the
  siblings of the hidden ones up to the root.
- **The checks run confined** by default (`[sandbox] mode = "auto"`):
  they write in `.kito/verify/` (the copy of the merged result, a
  temporary directory emptied before each run, a cache of their own:
  `TMPDIR`, `XDG_CACHE_HOME`, `GOCACHE`, `npm_config_cache`,
  `PIP_CACHE_DIR` point there); `.kito/`, the rest of a server's directory
  and the keys of the user are hidden; no network unless
  `network = true`. The result of an integration says whether its checks
  were confined, a failed check says that it ran confined, and
  `kito status` says how the checks of the repository run.
  `mode = "required"` refuses to run checks that cannot be confined.
- **`kito lane run <lane> -- <command>`** and the MCP tool **`run`**: a
  command in the working copy of a lane, confined to it (network allowed
  unless `--no-network`).
- **A key on the daemon's socket** (`.kito/socket.key`): the first line a
  client sends. Whoever can read `.kito/` can command the daemon; a
  confined command cannot read it, and so cannot. Without it the sandbox
  could be left through the socket.

### Demo

```bash
cargo build --release -p kitod -p kito-cli -p kito-server
```

```bash
scripts/demo-phase12.sh
```

Output: [`docs/demo/phase12-demo.txt`](demo/phase12-demo.txt). An excerpt
(a lane's check tries to leave its copy):

```text
(mallory) $ kito integrate 1
lane/1: verification failed: `sh check.sh`
  | payroll, another repository of this server:
  | who may connect to this server:
  | the key of this repository's daemon:
  | writing in the main directory of the shop:
  | calling out:
  | PermissionError: [Errno 13] Permission denied
  | the check itself, in its own copy:
  | ok
  | cat: /demo/server/repos/payroll/src/salaries.py: Permission denied
  | cat: /demo/server/server.toml: Permission denied
  | cat: /demo/server/repos/shop/.kito/socket.key: Permission denied
  | check.sh: 8: cannot create /demo/server/repos/shop/src/shop.py: Permission denied
```

And what a listener between a client and the server hears: 525 bytes of
noise from this server; from one started with `--plain`, the hello with
the token in full.

### What it costs

An integration with one check takes 36 ms confined and 36 ms not (median
of 20): the confinement is below what the measure can tell apart.
`kito lane run <lane> -- true` takes 10 ms. A new encrypted connection
with one request takes 5 ms, the same as one that is not encrypted, on
one machine.

### Tests

329 tests in all. New: a check that tries eleven things, confined, with
the network, and not confined (`kitod/tests/sandbox.rs`); `required` where
nothing can confine; the fingerprint however it is typed, a wrong one, a
relay that sees no token, a server on another machine with nothing to
vouch for it, `--plain` and what each side says when the other does not
match (`kito-server/tests/server.rs`); `kito lane run`, the key of the
socket and the fingerprint kept by `kito connect` from the command line;
the `run` tool.

### Known limitations

- **Linux only** (Landlock, 5.13; the network part 6.7). Elsewhere the
  checks run as before, and `kito status` says so.
- **No TCP is not no network**: UDP and Unix sockets are not confined. A
  check can still resolve names, and reach a local service that listens
  on a Unix socket.
- **A check that starts a local server on a port fails** without
  `network = true`: loopback is TCP too.
- **Tools that write caches in the home fail or complain** (cargo locks
  `~/.cargo`, for one): name the directories in `[sandbox] write`. A cache
  shared between checks can be poisoned by one of them, which is why none
  is writable by default.
- **Names are not hidden**: a confined command cannot read hidden files,
  but it can see that a path exists.
- **Not limited**: CPU, memory, disk and the number of processes; only
  the time a check may take.
- **The shell of an agent is not confined by itself**: `kito lane run` and
  the `run` tool confine what goes through them. Claude Code's own Bash
  tool is checked after the fact, as in phase 11.
- **A self-signed certificate does not expire here and is not rotated**:
  a new one means a new fingerprint for every client.
- **Clients are known by their token**, not by a certificate.
- **Two real machines were still not tried.**

## Phase 13 · integration without waiting in line — done

Integration was one lane at a time. With a type check on every
integration, ten agents waited a median of 11.6 s each (phase 3). The
checks are the wait: so lanes that ask while they run are now checked
together.

### What is there

- **The queue hands out several requests at once**
  (`kito-integrate/src/queue.rs`): every one that is waiting, up to
  `[verify] batch` (8). The first waits 15 ms for others made in the same
  breath, when the repository has checks.
- **`batch_plan`** (`kitod/src/integrate.rs`): each lane's merge is
  prepared against the main lane. The ones with their answer at once (a
  conflict, out of scope, nothing to integrate) get it. Of the others, the
  ones that stand alone (no rename, no move, no rename of the main lane
  that reached their code) and write files nobody before them in the run
  writes are members; their merged results are put in one copy.
- **One run of the checks** on that copy. Passed: each member is prepared
  again on the main lane as the members before it left it, and integrated
  only if what its merge writes is exactly what was checked; its report
  names the lanes of the run. Failed: the members are split in two, each
  half planned and checked again, down to one lane, which fails by itself
  with its own output.
- **What does not fit goes by itself**, with checks of its own: a lane on
  a file another member writes, a lane that renames or moves, a lane whose
  merge needs a question asked first, a lane asked for by somebody who may
  not integrate (it is merged, checked and left waiting), and any member
  that changed while it was being checked.
- **`kito integrate lane/1 lane/2 ...`** asks for all of them before it
  waits for the first, so that they are checked together.
- One driver (`integrate_batch`) for the daemon, which holds the
  repository only while planning and integrating, and for `Repo` used in
  place (`integrate_together`).

### Demo

```bash
scripts/demo-phase13.sh
```

Output: [`docs/demo/phase13-demo.txt`](demo/phase13-demo.txt). Five lanes
with a two-second check: 10.3 s one at a time, 2.1 s together.

### Acceptance target

Set before the phase: the median wait of ten agents under half of the
11.6 s of one lane at a time.

- **Met for lanes that do not depend on each other.** Ten agents, each on
  a file of its own, a check of 2 s, all asking at once: a median wait of
  2.1 s and 4.2 s for the last, with 2 runs of the check. One lane at a
  time on the same machine: 11.2 s and 20.3 s, with 10 runs.
- **Not met for lanes on the same files.** They are checked one after the
  other, as before: in the swarm of phase 3, where every agent writes the
  same shared file in every round, nothing changes. That run was not
  repeated: no lane of it can be a member of a batch.

### Tests

335 tests in all. New: three lanes in one run, a repository that wants
one at a time, a failing lane found among four in five runs, lanes on one
file and a lane that renames going by themselves, a lane saved again after
it was checked (`kitod/tests/batch.rs`); the queue handing out several
requests, and gathering one made a moment later; through the real daemon:
three lanes in one command, three commands at once, a failing lane among
three.

### Known limitations

- **Lanes on the same file are not checked together**, even on different
  functions. Checking them together needs the second merge to be made on
  the result of the first before either is integrated: the merge works on
  the main lane's recorded history, which the first lane is not part of
  until it is integrated.
- **Only the whole run is checked**, not each checkpoint in it: lane 2 of
  a run lands on a main lane that was checked with lanes 1 to 5 in it, not
  with 1 and 2 alone. The lanes of a run write different files, so each
  step is the checked result with files of later lanes as they were.
- **A failing lane costs runs**: found among four in five runs, where one
  at a time takes four. With checks that fail often, `batch = 1`.
- **The order inside a run is the order of asking**, and a lane that goes
  by itself waits for the run it was left out of.
- **No priorities**, and no way to take a lane out of a run that started.

## Phase 14 · git both ways — done

Kito pushed its history to a git remote and never forced a push. A commit
made there with plain git therefore stopped every push until somebody
reconciled the two by hand (a limit of phase 9). The first half of what
was asked for under "teams not all on Kito"; review and a web view are
phase 17 (lanes on lanes and permanent lanes came first: 15 and 16).

### What is there

- **`Repo::pull_import`** (`kitod/src/pull.rs`): given a commit that was
  fetched, it finds what the main lane lacks; opens a lane signed `git`
  from the checkpoint those commits were made on, declaring every file
  they change; and replays each commit of the first-parent line: its
  author on the ops, its subject as the intent, `git <hash>` as the task.
  A file is written as the commit left it when the lane has it as the
  commit found it; otherwise the change is carried over with a three-way
  merge by lines, and if that does not apply nothing is brought in.
- **A lane that brings commits** (`Lane.git_tip`) is integrated like any
  other. Its checkpoint records the commit (`Checkpoint.git_merge`), and
  the export makes a git merge with it as second parent.
- **Commits that change nothing here** (the same change was made on both
  sides, or every conflict was decided for the main lane) still join the
  histories: a checkpoint with no change of its own, exported as a merge.
  Without it the remote would refuse every push from then on.
- **The daemon pulls before it pushes**: `git fetch`, `pull_import`, the
  lane through the integration queue in its turn (checks included), then
  the push. Commits that wait (a conflict, a failed check) stop the push,
  and `kito status` says in which lane they wait.
- **`kito pull`** does the same when asked, and again after a conflict
  was decided. More commits that arrive meanwhile join the waiting lane.
- An event, `pulled`, for whoever watches.

### Demo

```bash
scripts/demo-phase14.sh
```

Output: [`docs/demo/phase14-demo.txt`](demo/phase14-demo.txt).

### Acceptance target

Set before the phase: a commit made outside Kito never blocks the push
again. Met for commits pushed to the remote on the branch Kito advances,
whose history starts from a commit the repository had: they are merged
and pushed back, or they wait in a lane for a decision and are pushed
once it is taken. Not covered: a commit made with git in the main
directory itself (still a direct edit), and a remote whose branch was
rewritten.

### Tests

339 tests in all. New: two commits merged function by function with a
lane's work, the merge commit and its parents, the push, the reasons of
the commits, a second round on top of the merge; a conflict that waits,
commits that join the waiting lane, the decision; nothing to bring and
nothing in common (`kitod/tests/pull.rs`); through a server with a real
remote: pull before push, a conflict, histories joined when nothing was
kept; `kito pull` from the command line.

### Known limitations

- **Only what is on the first-parent line of the remote's branch** is
  replayed, one step per commit: a branch merged there comes in as one
  change, under the merge's author and message.
- **Renames are a removal and an addition**: a file renamed with git
  loses its history in Kito at that point.
- **Each commit's author is on its changes; the merge is signed `git`.**
- **A commit made with git in the main directory** is not this: the files
  are recorded as edits made by hand (or put back, on a protected main
  lane), and the commit itself makes the export refuse to go on.
- **A remote branch that was rewritten** (force-pushed) is not followed.
- **Pulling happens before a push and when asked**, not on a timer: a
  remote that only receives commits made with git is not looked at until
  somebody integrates or runs `kito pull`.
- **`kito pull` on a server is for whoever runs it**: clients cannot ask.

## Phase 15 · lanes on lanes — done

Every lane started from the main lane and landed on it: an agent could not
build on another's work before it landed. The first half of
[ADR 0001](adr/0001-lanes-on-lanes.md); permanent lanes such as a
`staging` are phase 16, review and a web view phase 17.

### What is there

- **`Lane.parent` and `Lane.target`** (`kito-core`), optional: absent, a
  lane starts from and lands on the main lane, as every lane did. The
  store writes `kito-store 3` in its `format` file the first time a lane
  with either is recorded, so that an older binary refuses it instead of
  taking every lane for one of the main lane.
- **The state at any op** (`Repo::state_at`) is the replay of its first
  parents, back through its lane, the lanes below it and the main lane:
  it holds at any depth, and after a parent was brought up to its own
  target.
- **`open_lane_on`** (`kito lane open --on`, MCP `open_lane` with `on` and
  `target`): a lane on an open lane, as it is now, everything it saved
  included. Its target is the lane it is on, or one below it.
- **Integration onto a lane** (`kitod/src/integrate.rs`): the same merge,
  with the target where the main lane was. It writes in the target's
  working copy, records ops there signed by the agent that landed, makes
  no checkpoint and runs no checks; the target takes on the scope of what
  landed on it. The daemon does it as soon as it is asked, not behind the
  lanes waiting for the main lane's checks.
- **A lane lands only on a lane that holds what it was built on**
  (`Repo::lane_holds`: the op it started from, among the target's ops
  through both parents). A lane on `lane/1` that targets main waits until
  `lane/1` has landed, and then brings only its own work.
- **Closing a lane with lanes on it** is refused until what they were
  built on has landed; they are then moved where it went.
- **The why graph through parents**: what a stacked lane sees includes its
  parents' work up to where it started; a line that came to the main lane
  through a lane it landed on is told with the reason it was written for,
  by whoever wrote it.

### Demo

```bash
scripts/demo-phase15.sh
```

Output: [`docs/demo/phase15-demo.txt`](demo/phase15-demo.txt).

### Acceptance target

Set before the phase: an agent builds on a lane that has not landed, and
its work reaches the main lane with it or after it, never bringing work it
did not do. Met: a lane lands on the lane it started on, or further down
once that one has landed; three lanes deep land one into the other; a
parent with lanes on it closes only when their base has landed.

### Tests

350 tests in all. New: a lane on unlanded work, landing on it,
the parent landing both; a lane that targets the main lane waiting for its
parent and then bringing only its own work; closing refused, then the
child moved; three lanes deep; stacked lanes rebuilt from the log alone
and the store's format; why a stacked lane's code is the way it is, and
lines that came through a lane told with their own reason
(`kitod/tests/stack.rs`); the same from the command line.

### Known limitations

- **Landing on a lane runs no checks.** They run when the work reaches the
  main lane.
- **The uses of a renamed symbol are followed toward the main lane only**
  (level 2, phase 4): landing on a lane merges what the syntax tells.
- **A lane landed on keeps the reason of each landing as its own op**:
  `kito log` on the main lane shows those ops under the agent that landed
  them, and `kito why` tells the reason they were written for.
- **The lanes landed on are not exported to git**: only checkpoints of the
  main lane are, as before. Permanent lanes with branches of their own
  are phase 16.
- **Rolling the main lane back** below where a stacked lane's parent
  started is not looked at specially.

## Phase 16 · permanent lanes — done

The second half of [ADR 0001](adr/0001-lanes-on-lanes.md): a `staging`
next to `main`, with checkpoints, a git branch and checks of its own.

### What is there

- **`Lane.name`** marks a permanent lane; **`Checkpoint.lane`** says
  whose checkpoint it is (absent: the main lane). `Store::checkpoints()`
  still means those of the main lane, which every reader before assumed;
  `lane_checkpoints` and `all_checkpoints` are new. Either field makes the
  store `kito-store 3`.
- **`Repo::create_lane`** (`kito lane create staging [--from LANE]`, RPC
  `create_lane`): a lane with no scope, never closed, found by its name
  wherever a lane is named (`--on staging`, `kito integrate staging`).
- **Landing on a permanent lane** makes a checkpoint of it, runs its
  checks (`[lanes.<name>] verify`, else `[verify]`) and exports the
  checkpoint to its branch (`[lanes.<name>] branch`, else its name): each
  commit after the lane's commit before it and, the first time it is on a
  new state of main, after that state's commit. The branches are pushed
  with main's.
- **A permanent lane landing on main** (`kito integrate staging`) makes a
  checkpoint of main exported as a git merge with the lane's branch, and
  brings the lane up to main without closing it.
- **`kito lane catch-up staging`**: what main did since staging started
  from it is merged into staging, as a checkpoint of staging exported as a
  merge with main's commit. Staging keeps its base: those changes are on
  both sides when it lands, and merge as one. A conflict stops it whole.
- **`[lanes.<name>] integrate`**: who may land on it, checked when a
  server says who asks.

### Demo

```bash
scripts/demo-phase16.sh
```

Output: [`docs/demo/phase16-demo.txt`](demo/phase16-demo.txt).

### Acceptance target

Set before the phase: lanes land on a `staging` that has its own checks
and branch, CI can follow that branch, and staging reaches main as a
merge without being closed. Met, with what lands on main directly brought
into staging by `catch-up`.

### Tests

357 tests in all. New: names, closing refused, landing on staging
under its own checkpoint and branch, its own checks, staging landing on
main as a merge and staying, the next work on staging coming after main's
merge, catch-up and its refusal, who may land
(`kitod/tests/permanent.rs`); the same from the command line, with the
branch reaching a remote.

### Known limitations

- **Pulling a permanent lane's branch** from the remote (commits made on
  it with plain git) is not done: phase 14's pull is for main's branch.
- **Lanes landing on a permanent lane wait in the one queue** with those
  landing on main, and are not batched (phase 13 batches lanes landing on
  main).
- **A catch-up stops at the first conflict**: there is no conflict to
  decide, as there is for a lane; land staging first, or undo one side.
- **The directory of a permanent lane is a lane's working copy**: an
  edit made there by hand is recorded on the lane like any other, not
  put back as on a protected main.
- **The server's clients cannot create a permanent lane or ask for a
  catch-up**: those are done where the repository is.

## Phase 18 · the why graph, better — done

What the why graph tells is only as good as what it is given and how far
it follows. Done before phase 17 (review), at the person's request.

### What is there

- **Intents that say nothing are refused** (`kitod::lanes::says_nothing`):
  made only of words such as `wip`, `fix`, `update`, `minor`, `changes`,
  or with fewer than three letters, when a lane is opened or an intent is
  set. Intents Kito writes itself (git commit subjects, integrations,
  permanent lanes) are taken as they are (D-145).
- **Notes on code** ([ADR 0003](adr/0003-notes-on-code.md)): `kito note`,
  the MCP tool `note`, the daemon's `add_note`, `notes`, `remove_note`. A
  note is on a function (followed through renames and moves), a file or a
  directory; on lines, on the function they are in. Stored in
  `.kito/notes`, which an older Kito ignores. Shown as `annotations` with
  the why of that code, in the answers of `open_lane` and `extend_scope`,
  and to an agent about to change that code, once (D-146).
- **A file's history through its names** (`file_touches` in `why.rs`): the
  lines and reasons of a renamed file are followed under the names it had
  before; a new file under an old name no longer takes the history of the
  file that had it (D-147).

### Demo

```bash
scripts/demo-phase18.sh
```

Output: [`docs/demo/phase18-demo.txt`](demo/phase18-demo.txt).

### Tests

368 tests in all. New: intents refused and accepted
(`kitod/tests/lanes.rs`); notes on a function followed through a rename,
on files and directories, on lines, removed, kept across a restart,
refused when asked wrongly, and told once before an edit
(`kitod/tests/notes.rs`); notes from the command line; lines of a file
renamed once and twice, and a new file under an old name
(`kitod/tests/renamed.rs`).

### Known limitations

- **A rename made together with an edit** is a removal and a creation for
  Kito: the history starts again there.
- **Lines moved to another file with their function** (`Move`) keep the
  function's reasons, but not the reasons of each line.
- **The words that say nothing** are a list: an intent of real words that
  still says nothing ("do the thing") goes through.
- **Notes are not in git.**

## Phase 17 · review of a lane — done

A person had one thing to say about a lane: yes, by integrating it. Now
what a lane changes can be seen, things can be said about it where it is,
and a repository can ask for approvals before a lane goes in
([ADR 0004](adr/0004-review-of-a-lane.md)). Released as 0.19.0, after
phase 18. Kito has no interface for it: review is data in the store and
questions the daemon and the server answer, and a web view is another
program (decided with the person).

### What is there

- **The diff of a lane** (`kitod::review`, D-148): `lane_diff`, `kito
  lane diff`, the MCP tool `lane_diff`. The lane as it is against what it
  started from; per file the hunks and the symbols the lane added,
  changed, removed or renamed, with the lines each is on and the intents
  of the lane's ops on it. A rename or a move is one change. What the
  target got meanwhile is counted apart (`behind`).
- **Comments** (`kitod::comments`, D-149): on the lane, a function, a
  file or lines; answers; resolved, not removed. A comment on lines keeps
  their text and is found again by it; `outdated` once the lane no longer
  has those lines. Stored in `.kito/comments`, which an older Kito
  ignores. The first edit of the lane's agent after a comment is held
  back once with the comments it was not told; `lane status` shows the
  ones that wait.
- **Verdicts** (`kitod::verdicts`, D-150): approved, changes requested,
  withdrawn; the last one of a person on a lane stands. An approval is
  stale once the lane changes; the owner's does not count; an agent's is
  refused unless the repository lets agents review. Stored in
  `.kito/verdicts`.
- **What integration asks**: `[review] required`, `agents`, `keep`;
  `required` under `[lanes.<name>]`; `kito-server repo set --approvals`.
  A lane that lacks its review is merged and checked, then marked
  `awaiting_approval` with what is missing. Approving and integrating
  are two actions: an approval writes nothing to the target.
- **Who reviewed** is on the checkpoint (`reviewed_by`), in the why of
  that code, and in the exported commit as `Reviewed-by:` trailers.
- On a server, `read` sees diffs and reviews; `write` comments and gives
  verdicts; events `commented`, `comment_resolved`, `reviewed`.

### Demo

```bash
scripts/demo-phase17.sh
```

Output: [`docs/demo/phase17-demo.txt`](demo/phase17-demo.txt).

### Tests

390 tests in all, on Linux, macOS and Windows. New: hunks with their
context, files added, removed, renamed and binary, functions renamed and
moved to another file, paths asked, `behind` (`kitod/tests/review.rs`);
comments on a lane, a function, a file and lines, answers, resolution and
who may, lines that move and lines that change, the agent told once
(`kitod/tests/comments.rs`); a lane that waits for an approval, an
approval that brings nothing in, stale and kept approvals, requests for
changes, owners and agents, a permanent lane's own number
(`kitod/tests/verdicts.rs`); the three from the command line; a lane
reviewed on a server by roles, with the trailer in the commit
(`kito-server/tests/rules.rs`).

### Known limitations

- **No interface**: review is read and written from the command line, by
  agents through MCP, or by a program that asks the daemon.
- **On one machine there is one person**: whoever runs the repository.
  `required` greater than 0 is for a server, where people are known.
- **A comment on lines is found by their text**: lines that were changed
  by one character are no longer "those lines", and the comment is
  marked. Nothing guesses where they went.
- **Changes outside functions** (imports, text between them) are in the
  hunks of a diff and in no symbol.
- **The diff is the lane against its start**, not against its target as
  it is now: what a catch-up would bring or collide with is not shown.
- **The rules written for agents** by `kito init` do not mention review
  yet: an agent learns of comments when it is told them.
- No owners of paths whose approval is required for those paths.

## Phase 19 · plugins — done

Kito had one way to grow: change Kito. A web view, another coding
assistant, a team's own rule for integration each meant that. Now they
are programs of their own ([ADR 0005](adr/0005-plugins.md)). Released as
0.20.0.

### What is there

- **A plugin is a program** (`kitod::plugins`): `kito-<name>` and a
  `kito-plugin.toml`. Installed for a person (`plugins/` in their Kito
  directory), enabled by a repository (`[plugins.<name>]`, with what it
  was given). `kito plugin add --path | enable | disable | list | info |
  remove`.
- **A key and permissions** (D-151): the first line on the daemon's
  socket is a key; a plugin's own makes the connection that plugin's, and
  every request is checked against `read`, `write`, `integrate`, `admin`
  as the repository gave them. A method the list does not know needs
  `admin`; `plugin_key` and `shutdown` are for no plugin.
- **The daemon's API, version 1** ([`docs/api.md`](api.md)): every method
  under the permission it needs. A test reads the document and the
  daemon's source and fails when they disagree.
- **Commands**: `kito <name>` for a name Kito does not have runs the
  enabled plugin that declares it, with `KITO_REPO`, `KITO_API` and
  `KITO_PLUGIN_KEY`.
- **Gates** (D-152): asked with the checks, in the same confined copy,
  in the same run when lanes are checked together; pass, fail or wait.
- **Events** (D-152): a thread hands each event to a queue per plugin;
  what a plugin last failed at is kept until it runs well again.
- **Tools for agents** (D-153): `kito-mcp` lists the tools of the plugins
  enabled, as they are when asked, as `kito.<plugin>.<tool>`.
- **Assistants** (D-153): `kito assistant add | remove | list` run the
  plugin's own setup. No assistant plugin exists yet.

### Demo

```bash
scripts/demo-phase19.sh
```

Output: [`docs/demo/phase19-demo.txt`](demo/phase19-demo.txt).

### Tests

402 tests in all, on Linux, macOS and Windows. New: permissions of
methods, manifests, a repository's word written in its configuration
(`kitod/src/plugins.rs`); the API document against the daemon
(`kitod/tests/api.rs`); from the command line, with a real plugin
(`kito-hello`, an example of `kito-cli`): installed, enabled, answered
what it was given and refused the rest, given less, disabled, removed;
what is not a plugin; a gate that passes, fails, says to wait, says
nothing, never ends; events in order with a failure among them; an
assistant set up and removed (`kito-cli/tests/cli.rs`); a plugin's tools
listed and called by an agent, with and without a dot in their names
(`kito-mcp/tests/mcp.rs`).

### Known limitations

- **No plugin is installed by name**: only from a directory. Downloading
  from a release waits for a plugin that has one.
- **A plugin runs where the repository is.** The daemon of a directory
  connected to a server does not answer plugins; on a server, gates and
  events are those of the repository there.
- **Gates when lanes are checked together, and on a server**, go through
  the code that is tested for one lane on one machine, and have no test
  of their own.
- **Plugins are trusted programs.** The permissions limit what the daemon
  answers a plugin; they do not confine what its program does on the
  machine. Only gates run confined, and only where the checks do (Linux).
- **What a plugin last failed at** is forgotten when the daemon stops.
- **Claude Code is still set up by Kito itself**, not by a plugin.
- No registry of plugins, no signatures beyond a release's checksum.

## Phase 20 · plugins for the server — done

A team's server had people with tokens, and nothing else to grow with:
no sign-in, no pages, no way to hear what happened on it. Now those are
plugins too ([ADR 0006](adr/0006-server-plugins.md)), the same programs
with the same manifest, enabled by a server. Released as 0.21.0.

### What is there

- **A server enables plugins** (`kito-server plugin add --path | enable
  | disable | list`, `[plugins.<name>]` in `server.toml`), and gives
  what is a server's to give: `sign_in`, `pages`, `server_events`,
  `server_admin`. A repository still gives only its own.
- **Commands**: a `[[server_command]]` is a command of `kito-server`,
  run with the server's directory in `KITO_SERVER_DIR`.
- **Events**: `repo_added`, `repo_changed`, `user_added`,
  `user_removed`, `role_changed`, `signed_in`, `sign_in_refused`,
  `session_ended`, `plugin_failed`, told after the fact by whoever did
  the thing (D-154). A plugin that fails is said to the administrator
  and in `plugin list`, and undoes nothing.
- **The HTTP address** (`serve --http`, D-156): `/-/me`, `/-/session`,
  `/-/api/<repository>` (the daemon's API, asked as the person, within
  their role and within what the page's plugin was given). Every other
  path is passed on to the plugin whose `[pages]` are mounted there,
  started by the server and told who asks.
- **Signing in** (D-157): `kito-<name> sign-in begin | finish | lookup`
  say who somebody is; the server keeps `<method>:<subject>` on a user
  (`user add --identity <method>:<login>`) and decides everything else.
  `kito connect --sign-in <method>` and `kito sign-in` for the command
  line, `/-/sign-in/<method>` for a browser. Sessions are the server's
  (`sessions.toml`, hashed, thirty days by default).
- `kito-hello` is a provider that does not exist, a page, an event
  listener and a command, for the tests and the demo.

### How it was tested

- `kito-server/tests/plugins.rs`: enabling and its permissions, a
  plugin's commands, the events in order, a plugin that fails.
- `kito-server/tests/http.rs`: who asks by token and by session, the
  API within a role, pages passed on with who asks and without their
  token, HTTP in the clear only on this machine, HTTPS with the server's
  certificate; signing in in a browser from the way out to the session,
  the browser that did not leave, the person the server does not have,
  the one who said no, sessions ended by closing and by removing the
  person, what the plugins were told.
- `kito-cli/tests/cli.rs`: `kito connect --sign-in` waiting until
  somebody has signed in, a way and a person the server does not have,
  a session that ended and `kito sign-in` after it.
- `scripts/demo-phase20.sh`, kept in `docs/demo/phase20-demo.txt`.

### Known limitations

- **No plugin signs in with a real provider yet.** `kito-github` is
  next; until then only `kito-hello` answers, which is for tests.
- **A sign-in plugin is trusted for who it says somebody is.** One that
  is wrong lets the wrong person in as a user the server has; it cannot
  make a user, or give a role.
- **Sessions are not listed or ended by an administrator** one by one:
  removing the person ends theirs, and so does `sessions.toml` removed.
  A session that runs out is dropped without `session_ended` being told.
- **The directory's daemon is restarted by `kito sign-in`**: it holds
  the session it started with.
- **Signing in is for a server reached as host:port.** Through SSH a
  person is who their key says.
- **One plugin's pages cannot be told from another's** by the server:
  pages together are held to what all of them were given (D-156).
- **Not run on macOS or Windows** since 0.20.0: the project's CI has
  been stopped by the account's billing, not by a failure.

## Phase 21 · a team's kitchen — done

Kito told an agent the rules of version control and nothing of how a
team works: that was copied by hand into every repository's `CLAUDE.md`
or `AGENTS.md`, for every assistant, and stale in most. A kitchen is one
place for it, on the team's server ([ADR 0007](adr/0007-kitchens.md)).
Released as 0.22.0; the kitchen itself is the plugin `kito-kitchen`.

### What is there

- **Plugins in a connected directory** (D-158): the ones the server
  enabled for its clients (`--for-clients`), with what of a repository
  the server gave them, for whoever has them installed. The directory's
  daemon knows a plugin by its key and refuses it the rest; a person
  enables nothing there.
- **`plugin_call`** (the daemon's API, version 2): a plugin asks the
  plugin of its own name on the server, which runs `kito-<name> answer
  <question>` and is told who asks, about which repository, with which
  role.
- **A plugin's say when a session starts** (D-159): `[session]` in a
  manifest, `kito-<plugin> session`, given after Kito's own rules.
- **`kito-kitchen`** (its own repository): a kitchen is a repository on
  the server with `behaviour.md` (always told), `concepts/` and
  `recipes/` (named, one line each, the text asked for), `tools/` (a
  command or a script every agent has, run in the lane's working copy)
  and `parts/` (the same, for the repositories that pick them).
  `kito-server kitchen new | set | parts | show | check`; `kito kitchen
  [concept | recipe <name> | run <tool> --lane <lane>]`; for agents the
  tools `kitchen`, `concept`, `recipe`, `run`, and the kitchen at the
  start of a session; a copy in the directory for when the server
  cannot be reached.

### How it was tested

- `kito-cli/tests/cli.rs`: a connected directory runs the plugin the
  server let it and no other, the plugin is refused what it was not
  given, asks its counterpart and only its counterpart, and is told a
  failure of the server's plugin as one; a plugin's words at the start
  of a session come after Kito's rules, and only when it is enabled.
- `kito-kitchen`'s own tests: a kitchen's files and what is wrong with
  them, the server's side alone, and one under `kito-server` and `kito`
  themselves: a person and an agent get the kitchen, a part wins over
  the base, a tool runs in the lane with what it is given as one word,
  ends badly, is stopped when it takes too long; the session's start;
  what is integrated into the kitchen is the kitchen at once; the
  server away.
- `scripts/demo-phase21.sh`, kept in `docs/demo/phase21-demo.txt`.

### Known limitations

- **A kitchen's tool is not confined**: it runs as the person, in the
  lane's working copy. Who may change the kitchen may run commands on
  the team's machines; `KITO_KITCHEN_NO_TOOLS` turns them off for a
  person.
- **A kitchen's tools are behind one tool, `run`**: what a plugin offers
  agents is fixed in its manifest.
- **Events and gates of a plugin do not run in a connected directory**:
  they are the repository's, on the server.
- **What the server says of its plugins is believed for fifteen
  seconds** by a directory's daemon.
- **Which repository is the kitchen, and what each picks, have no
  history**: they are a file of the plugin on the server.
- **No kitchen without a server**, and none shared between servers.
- **Not tried with an assistant in a real session**: the tools and the
  session's start are run as Kito runs them, by tests.
- **Not run on macOS or Windows** since 0.20.0: the project's CI is
  stopped by the account's billing. A kitchen's scripts are for a Unix
  shell.

## Phase 22 · a registry of plugins — done

A plugin was installed from a directory somebody had: with eight of them
and a server that tells people "install `kitchen`", there was nowhere to
get one from, no clean way to take one out, and a server's plugins could
only be changed on its machine. Released as 0.23.0
([ADR 0008](adr/0008-plugin-registry.md)).

### What is there

- **The registry is files** (`kitod::registry`, D-160): `index.json` and
  `plugins/<name>.json`, read over HTTPS, from a directory (`file://`),
  or over HTTP on this machine only. `https://kito.sh/registry` unless
  `KITO_REGISTRY` says another.
- **By name**: `kito plugin add <name>[@version]`, `search`, `outdated`,
  `update`; `kito-server plugin add <name>`. The archive is checked
  against the registry's SHA-256, unpacked apart as something hostile
  (plain files inside its own directory only), and installed only if its
  manifest is the plugin and the version that were listed.
- **Taking a plugin out** (D-161): `[cleanup]` in a manifest, run one
  last time where the plugin goes (`cleanup repository | server |
  person`); `disable` removes its key and, with `--purge`, what it kept;
  `remove` runs the person's cleanup; `kito-server plugin remove`. A
  cleanup that fails stops nothing.
- **A server's plugins from a browser** (D-162): administrators
  (`kito-server user admin`), `/-/admin/plugins`, the events
  `plugin_installed | enabled | disabled | removed`.
- Outside this repository: the page in `kito-web` 0.5; `[cleanup]` in
  the plugins for Codex, Claude Code and ZCode; in the website's
  repository, `scripts/registry-add.mjs` (lists a plugin from an archive
  of it) and `install.sh`, on a branch that is not published.

### How it was tested

- `kitod::registry`'s own tests: which version is picked, an archive
  that tries to leave its place, what is read and what is not.
- `kito-cli/tests/cli.rs`: a plugin installed by its name from a
  registry of files; a wrong checksum, a wrong version, a name the
  registry lacks, a version for a later Kito; a later version found and
  installed; a plugin that cleans up when disabled, purged, removed, and
  one whose cleanup fails.
- `kito-server/tests/plugins.rs`: the same on a server, and `remove`
  refused while enabled.
- `kito-server/tests/http.rs`: `/-/admin/plugins` for an administrator,
  for somebody who is not, for a page of another site; only by name;
  install, enable with less than asked, disable with purge, remove; the
  plugin that serves the pages.
- By hand, in a browser: the page of `kito-web` against a demo server,
  installing and enabling a plugin from a registry of files.
- `scripts/demo-phase22.sh`, kept in `docs/demo/phase22-demo.txt`.

### Known limitations

- **Nothing is published.** The registry at kito.sh is empty, and the
  site that would serve it is not deployed: Kito and its plugins are
  private. Everything above was run against registries of files.
- **No signatures.** The checksum ties a download to what the registry
  listed; it does not say the registry was right. Who can write the
  registry decides what a name installs.
- **A registry is named by an environment variable**, not yet in a
  configuration file of a person or of a server.
- **A plugin's pages and way to sign in change when the server starts
  again**, not when it is enabled or disabled from the browser.
- **An administrator's session is as good as the administrator**: it
  can install whatever the registry lists. A server that wants none of
  it names nobody.
- **`kito-server` has no `outdated` or `update`**: `plugin add <name>`
  again installs the latest.
- **The hooks' test once let an edit through under load** that should
  have been held back. It was not reproduced (twelve runs on a busy
  machine), and now says what Kito knew when it happens.
- **Not run on macOS or Windows** since 0.20.0: the project's CI is
  stopped by the account's billing. Archives are unpacked with `tar` in
  the tests.

## Phase 23 · the registry is signed — done

The registry tied a download to what it listed, and said nothing of who
the registry is: whoever served its files, or carried them, decided what
a name installs. Released as 0.24.0
([ADR 0009](adr/0009-signed-registry.md), D-163).

### What is there

- **A key for a registry** (Ed25519), and a signature beside each of its
  files. `kito registry key`, `kito registry sign`.
- **Checked when the key is known**: `KITO_REGISTRY_KEY` for any
  registry, a key written in Kito for the one at kito.sh (none yet). A
  file not signed by it is refused, and nothing of the registry is used.
- **No going back**: a numbered index, remembered per registry on each
  machine; a plugin's file held to the index's latest version.
- `plugin add` says whether the registry was checked; so does
  `/-/admin/plugins`.

### How it was tested

- `kitod::registry`: a signed registry taken; another's key, a changed
  file, a missing or unreadable signature refused; one of two keys; an
  old plugin file and an old index, signed as they were, refused; a
  registry whose files do not agree not signed.
- `kito-cli/tests/cli.rs`: the same from the command line, with a key
  made by `kito registry key` and a registry signed by `kito registry
  sign`; a key that cannot be read.
- `scripts/demo-phase23.sh`, kept in `docs/demo/phase23-demo.txt`.

### Known limitations

- **The registry at kito.sh has no key yet.** Making it is its keeper's
  to do; until it is written in Kito, that registry is not checked.
- **A machine's first look cannot tell an old index from the current
  one.** The number protects from the second look on.
- **A lost secret means a new key**, told to everybody again.
- **Keys have no end date**, and one signer is enough.
- **Not run on macOS or Windows** since 0.20.0: the project's CI is
  stopped by the account's billing.

## Phase 31 · instructions, for the web client — done

The web client's instructions page (kito-web 0.12) shows what each
assistant reads and who changed what, as the person asked. Released as
0.32.0 (ADR 0012, amended).

### What is there

- Kito's rules, and the writing of what an assistant reads, are the
  daemon's: `instructions_text`.
- Each change of an instruction is kept, with who made it and the text
  before and after: `instructions_history`.

### How it was tested

- `kitod` unit tests: what an assistant reads, the history.
- `kito-server/tests/server.rs`: what Claude Code reads through the
  server, and the history, for one who may only read.
- kito-web's `tests/team.rs`: written by one who may write, read by a
  reader, refused to the reader.
- The page, against kito-web's `scripts/demo-team.sh`, in a browser.

### Known limitations

- **The history is the last 200 changes.**
- **Not run on macOS or Windows** since 0.20.0.

## Phase 30 · a repository's rules, for the web client — done

The web client's settings page (kito-web 0.12) changes a repository's
rules as its maintainers may. Asked for with it: checks for admins only,
since they run commands on the server; and who changed what. Released
as 0.31.0 (ADR 0014, amended).

### What is there

- `configure` with `checks`, through the server, for admins only.
- A kitchen given through the server must be another of its
  repositories.
- `server.settings_history`: who changed which rule, and to what.

### How it was tested

- `kito-server/tests/server.rs`: a maintainer refused the checks and
  any kitchen that is not another repository; an admin who sets the
  checks; the history in order, with `repo set` on the server's machine.
- The page, against kito-web's `scripts/demo-team.sh`, in a browser.

### Known limitations

- **The history is the last 200 changes** of each repository; the audit
  log of the server is still to come.
- **Not run on macOS or Windows** since 0.20.0.

## Phase 29 · people, for the web client — done

The web client's people page (kito-web 0.11) needed what the server
did not give: who uses a repository over HTTP, when each was last here,
and roles that count at once. Released as 0.30.0 (ADR 0014, amended).

### What is there

- An open connection reads the person's role at every request.
- `seen/<name>`: when each person was last here.
- `POST /-/people`; `server.members` and `server.grant` through
  `/-/api/<repository>`; `server.members` for maintainers and admins.
- Adding somebody answers the port and whether the server encrypts.

### How it was tested

- `kito-server/tests/server.rs`: a role lowered and a repository taken
  away in an open connection; the members with when each was here.
- kito-web's `tests/team.rs`: the people and the members over HTTP, for
  an admin and for one who may only read.
- The pages, against `scripts/demo-team.sh` of kito-web, in a browser.

### Known limitations

- **Last seen** is to the minute.
- **Not run on macOS or Windows** since 0.20.0.

## Phase 28 · roles per repository and on the server — done

A person had one role for every repository they could use, and the
server had an `admin` flag for its plugins. The person asked for
permissions per repository, roles on the server, and simple commands.
Released as 0.29.0 ([ADR 0014](adr/0014-roles.md), D-168).

### What is there

- Roles in a repository: `read`, `write`, `integrate`, `maintain`
  (its rules and its people). Roles on the server: `member`, `admin`,
  `owner`.
- `kito-server grant <name> <repo> <role>`, `revoke <name> <repo>`,
  `user add --repo shop[:role]… --role <server role>`, `user role`.
  `user admin` is gone.
- In a connected directory: `kito members`, `kito grant`, `kito revoke`;
  `kito people [add|remove|role]` for admins. A maintainer may
  `protect` and `configure` through the server.
- A server written before is read as it was meant and written back.

### How it was tested

- `kito-server/tests/server.rs`: a member given nothing; a maintainer
  who lists, grants, revokes and configures; an integrator refused
  `configure`; an admin who manages people and may not name admins; an
  owner who may; an old `server.toml` read as it was meant.
- The tests of the server, the client, kito-mcp, kito-web, kito-kitchen,
  kito-github and kito-oidc, moved to the new commands.
- `scripts/demo-phase28.sh`, kept in `docs/demo/phase28-demo.txt`.

### Known limitations

- **Adding and removing repositories** is on the server's machine only.
- **A change of role counts from the person's next connection.**
- **The demos of phases 20 to 22** use `user admin` and `--role write`,
  which are gone; their kept output stays as it was.
- **Not run on macOS or Windows** since 0.20.0.

## Phase 27 · connecting by one address — done

Connecting needed the server, the repository, a token or a way to sign
in, and a fingerprint. The person asked for `kito connect <url of repo>`
with the browser opening by itself, and `kito clone`. Released as 0.28.0
([ADR 0013](adr/0013-connect-by-address.md), D-167).

### What is there

- `kito connect kito.example.com/shop` (`kito://host:port/shop`; 7421
  by default), and `kito clone`, the same command.
- No token given: the server says how to sign in; at a terminal the
  person picks, or pastes a token; the browser opens on the page.
- A self-made certificate of another machine is shown and asked about
  at a terminal; a public one is known by its name without a flag.

### How it was tested

- Unit tests of the address forms.
- `kito-cli/tests/cli.rs`: `kito clone kito://<server>/shop` with no
  token and no `--sign-in`, signed in the only way the server offers.

### Known limitations

- **The questions at a terminal and the browser are not tested** by the
  suite: they need a terminal. With nobody there, nothing changed.
- **Not run on macOS or Windows** since 0.20.0.

## Phase 26 · instructions as metadata — done

What agents were told lived in `CLAUDE.md` and `AGENTS.md`, recorded and
exported to git like code, and each person's copy could differ. The
person asked that such files be kept out of the repository, shared
through Kito and its server instead, by one standard, with a kitchen
attached and the repository's own instructions over the kitchen's.
Released as 0.27.0 ([ADR 0012](adr/0012-instructions.md), D-166).

### What is there

- **Instructions**: `all/<name>.md` for every assistant,
  `<assistant>/<name>.md` for one, kept in `.kito/instructions/`; a
  kitchen's `instructions/` come first, the repository's own win, an
  empty one takes the kitchen's away.
- **`kito instructions`** lists them and where each comes from; `show
  --for <assistant>`, `set <name> [--file]`, `remove <name>`, `kitchen
  <dir> | --none`, `write`.
- **`CLAUDE.md` and `AGENTS.md` are written by Kito** from them, between
  its marks, when an assistant is added or removed, when one changes, and
  at every session start. What a file said before becomes an instruction.
- **Out of the history and of git by default**: `CLAUDE.md`, `AGENTS.md`,
  `GEMINI.md`, `.mcp.json`, `.claude/`, `.codex/`, `.zcode/`, `.cursor/`
  and the others; `.gitignore` names them. `[agents] files_in_history =
  true` keeps the old way.
- **Through a server**: a connected directory reads and changes the
  server's instructions, by the person's role; `kito-server repo set
  <repo> --kitchen <repo>` attaches a kitchen kept on the server.
- **kito-kitchen 0.4.0**: a new kitchen starts with `instructions/`.
- **kito-claude 0.1.2**: taking Claude Code out takes Kito's marked part
  of `CLAUDE.md` with it.

### How it was tested

- `kitod` unit tests: names, layers, overriding and taking away.
- `kito-cli/tests/cli.rs`: init leaves the files out of the history and
  of git; a `CLAUDE.md` that was in git becomes an instruction; a kitchen
  directory, an override, a removal, `show --for codex`, lines outside
  the marks kept, the file taken away with the assistant.
- `kito-server/tests/server.rs`: a kitchen kept as a repository of the
  server, an override seen by another person, a reader refused a change.
- kito-kitchen, kito-codex, kito-zcode, kito-claude: their tests under
  this Kito.
- `scripts/demo-phase26.sh`, kept in `docs/demo/phase26-demo.txt`.

### Known limitations

- **Instructions have no history**: an event says each change, and the
  last text is all there is.
- **A file already in the history stays recorded** until it is removed
  from the project in a lane.
- **Written at session start**: an assistant that read its file before
  the hook ran sees a change at its next session.
- **A kitchen on a server is read as its main directory is**, without
  the parts kito-kitchen picks for each repository.
- **Not run on macOS or Windows** since 0.20.0.

## Phase 25 · one way to add an assistant — done

Setting up an assistant took a different set of commands for each, and
the assistants already on a machine were not looked for. Released as
0.26.0 ([ADR 0011](adr/0011-one-way-to-add-an-assistant.md), D-165).

### What is there

- **`kito assistant add <name>…`** for every assistant: Claude Code by
  Kito, the others by their plugin, installed from the registry and
  enabled here when needed, each step said. `remove` takes out what
  `add` wrote, Claude Code's included.
- **`kito assistant list`**: the assistants Kito knows, whether this
  machine has each (its program on the `PATH`, or its settings
  directory), and how each is set up.
- **`kito assistant default <name>… | --none`**: which assistants every
  new project gets, in the personal configuration.
- **`kito init`, `import`, `connect`** set those up; with nothing said
  they ask at a terminal, and elsewhere set up Claude Code and name the
  others found. `--assistant <name>`, `--no-assistants`.
- **kito-zcode 0.2.0**: `add zcode` also writes Kito's hooks in the
  person's own ZCode settings.
- **install.sh** asks, for each assistant found, whether new projects
  should get it.

### How it was tested

- `kito-cli/tests/cli.rs`: an assistant set up by an installed plugin
  that was not enabled; one the registry cannot give; a machine with
  Codex's program on the `PATH` and ZCode's settings in its home
  directory, listed; `init` with nobody at the terminal, with defaults,
  with `--assistant`, `--no-assistants`, `--no-claude`; Claude Code
  removed and added again.
- kito-zcode, kito-codex and kito-claude: their tests under this Kito.
- `scripts/demo-phase25.sh`, kept in `docs/demo/phase25-demo.txt`.

### Known limitations

- **The question at a terminal is not tested** by the suite: it needs a
  terminal. The paths around it are.
- **Detection knows three assistants**, by three directories and three
  programs. Any other is added by name.
- **Installing a plugin from the registry during `init`** needs the
  registry to be reachable and to have it; otherwise `init` goes on and
  says so.
- **Not run on macOS or Windows** since 0.20.0.

## Phase 24 · which assistant works in a lane — done

A lane said who works in it by a name and a person, and not what that
agent is. The person who watches several lanes asked to see the
assistant on each, and the model where it can be known. Released as
0.25.0 ([ADR 0010](adr/0010-assistant-in-a-lane.md), D-164).

### What is there

- **A lane keeps an `assistant`**: a name, and a version and a model
  when known. In `lanes`, `lane_status`, `board`, the answer of
  `open_lane`; `kito lanes` and `kito board` say it (`in claude-code
  (claude-opus-5)`).
- **Said through the MCP server**: the name and version the program gave
  when it connected, and the model if the agent gives `open_lane` one.
- **Said by the hooks**: `kito hook session-start` says what the session
  runs on (Claude Code gives its model then), `kito hook before-edit`
  which session an edit is of, and the lane the edit is in keeps both.
  Another assistant's hooks say `--assistant NAME` (`--model`,
  `--session`). Claude Code is guessed from the mark it puts on what it
  runs: the guess names a lane nobody named, and is dropped, with its
  model, where a program named itself otherwise.
- **Said at every question about a lane** by the MCP server: a lane
  taken over by another assistant says so from its first question.
- **Said by hand**: `kito lane open --assistant NAME [--model M]`.
- The same program completes what is known; another replaces it.

### How it was tested

- `kito-core/tests/model.rs`: what is kept and what is dropped, the same
  program under two names, another program, a session's word completed.
- `kito-cli/tests/cli.rs`: from the command line and through the hooks
  as Claude Code runs them: the model of a session reaching the lane its
  edit is in, a session that said nothing, an assistant that is not
  Claude Code, Claude Code's mark in a lane named for another program, a
  lane taken over, a daemon started again.
- `kito-mcp/tests/mcp.rs`: the program's name and the agent's model
  through the tools; a lane named for another program, asked about; and
  through a server, with a session's word joined there.
- `kitod/tests/api.rs`: `assistant_session` is written down under `read`.
- `scripts/demo-phase24.sh`, kept in `docs/demo/phase24-demo.txt`.

### Known limitations

- **Nothing checks it.** It is what the assistant's side said.
- **Claude Code's model is known from a session's start**, with the
  daemon running: a model changed in the middle of a session is not
  seen, and a daemon that restarted has forgotten the sessions.
- **Only the last assistant of a lane is kept**, not those before.
- **No real assistant was run for it**: the hooks were given what Claude
  Code's and Codex's documentation say they write (read 2026-11-21).
  What each names itself to an MCP server is not in their documentation.
- **Not run on macOS or Windows** since 0.20.0: the project's CI is
  stopped by the account's billing.

## After phase 5

- **0.5.1 · ready for Claude Code from `kito init`** (D-072). `init` and
  `import` write what Claude Code needs to work through Kito: the server in
  `.mcp.json`, the permission in `.claude/settings.json`, the rules in
  `CLAUDE.md`, `.kito/` in `.gitignore`; `kito claude` does it later.
  Tried with Claude Code itself on a project that had only `kito init` run
  in it: asked in plain words to add a function, and to rename one, it
  opened a lane, worked in it, integrated and closed.
- **0.5.2 · a shorter `kito log`** (D-073). One line for each run of saves
  of a file, with what they meant for its symbols on the same line; the
  longer form, every save on a line of its own, is `kito log -v`.
