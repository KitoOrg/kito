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
  be tried here.
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
  untried, and the server still encrypts nothing.
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
  wrote, with no sandbox: phase 10.
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
