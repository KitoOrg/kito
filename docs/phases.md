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

## Phase 2 · level 1 semantics — to do

## Phase 3 · lanes and integration — to do

## Phase 4 · level 2 semantics — to do

## Phase 5 · coverage and performance — to do
