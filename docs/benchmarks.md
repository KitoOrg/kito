# Benchmarks

The plan asks to optimize only with a `criterion` benchmark that proves the
gain. The measurements that justified an optimization are recorded here.

They are run with:

```bash
cargo bench -p kito-store
```

The measurements below come from a 4-core VM with a virtual disk: they are
valid as a before/after comparison, not as absolute values.

## Phase 0 · reusing the zstd contexts

`zstd::bulk::compress` and `decompress` create a context on every call. For
blobs of a few hundred bytes that cost dominated everything else.

| Benchmark | Before | After | Change |
| --- | --- | --- | --- |
| `write/put_blob_512_bytes` | 24.8 µs | 9.4 µs | −62% |
| `read/get_blob` | 20.4 µs | 4.1 µs | −81% |
| `write/append_1000_ops_then_commit` (1000 ops, 2000 blobs) | 50.7 ms | 26.4 ms | −41% |
| `write/put_blob_65536_bytes` | 445 µs | 388 µs | within noise |

`Packs` now holds a `Compressor` and every reader thread a `Decompressor`.

## Phase 0 · reference values

| Benchmark | Measurement |
| --- | --- |
| `write/append_1_op_then_commit` (1 op, 2 blobs, 2 fsyncs) | 0.56 ms |
| `read/get_op` | 3.7 µs |
| `read/lane_ops_page_of_100` | 172 µs |
| `recovery/open_clean_50k_ops` | 6.4 ms |
| `recovery/open_and_rebuild_index_50k_ops` | 281 ms |
| `recovery/verify_50k_ops` | 1.2 s (before the reuse of the zstd contexts) |

## Phase 1 · save → op latency

End-to-end measurement with the real daemon, a 2,000-line file (172 kB), one
line changed on every save, 300 saves:

```bash
cargo run --release -p kitod --example latency -- --saves 300 --lines 2000
```

| Measurement | p50 | p95 | max |
| --- | --- | --- | --- |
| Save → confirmed op, as seen by the client | 54.9 ms | 56.7 ms | 91.1 ms |
| First event → confirmed op, measured by the daemon | 53.8 ms | 55.5 ms | |
| Processing alone, after the debounce | 3.1 ms | 4.7 ms | |

The debounce is 50 ms. See the discussion of the "< 30 ms at p95" target in
[`phases.md`](phases.md).

## Phase 2 · the cost of symbols on every save

```bash
cargo bench -p kito-diff
```

The first version parsed both sides of every save from scratch and looked up
each old symbol with a scan of the table: on a file of 2,000 functions the
processing of a save went from 3 ms (phase 1) to 127 ms.

| Step, file of 2,000 functions (172 kB), one changed | Before | After | What changed |
| --- | --- | --- | --- |
| Matching old and new symbols (`diff_one_function_changed_of_2000`) | 33.4 ms | 1.1 ms | old symbols looked up by (parent, kind, name) instead of scanning |
| Reading the new side (`reparse_2000_functions_one_changed`) | 42.8 ms | 21.8 ms | incremental parse from the cached tree of the old side |
| Reading the old side | 42.8 ms | 0 | taken from the parse cache |

`parse_2000_functions` (from scratch, 42.8 ms) is the reference for the first
touch of a file. Of the 21.8 ms of an incremental reparse, about 1 ms is the
parse itself: the rest is running the symbols query and hashing the tokens
of the whole file again, which is what a later optimization can avoid.

End to end, with the real daemon (`--shape typical` is an ordinary source
file, `--shape dense` the worst case of one function per line):

```bash
cargo run --release -p kitod --example latency -- --shape typical
```

| File | Processing p50 | p95 |
| --- | --- | --- |
| 2,000 lines, 143 functions (58 kB) | 14.1 ms | 20.9 ms |
| 2,000 lines, 2,000 functions (172 kB) | 31.5 ms | 50.5 ms |

## Phase 3 · integration and lane creation on a large repository

```bash
cargo run --release -p kitod --example scale -- --files 50000
```

A repository of 50,000 TypeScript files of 1 kB (250,000 symbols). A lane
changes one function in each of 50 files, which is 100 ops (a file-level op
and a symbolic one per file), and is integrated; the verifier is off, and so
is the export of each checkpoint to git, which is measured further down.

| On 50,000 files | Before | After |
| --- | --- | --- |
| Integrating 100 ops | 2,119 ms | 90 ms |
| Integrating 1,000 ops | 3,753 ms | 299 ms |
| Integrating 100 ops from a lane the main lane left 550–650 files behind | 754–919 ms (measured halfway through the work) | 123–136 ms |
| Creating a lane (plain copies, ext4) | 1,433–1,840 ms | 462–561 ms |

Where the 2,119 ms of the first row went, step by step:

| Step | Before | After | What changed |
| --- | --- | --- | --- |
| Scanning the lane's working copy | 168 ms | 60 ms | the file system is asked about the files by four threads; only what the stat cache does not vouch for is looked at |
| Scanning the main directory, before the merge | 177 ms | 3 ms | only the files the lane touched are looked at (D-048) |
| Merge | 177 ms | 5 ms | reads only the files involved; the three versions come from the parse cache |
| Scanning both again, before writing | 358 ms | 2 ms | only the files about to be written |
| Writing and recording the merged files | 46 ms | 20–30 ms | — |
| Saving the state of the main lane | 250 ms | 0 | saved when the daemon stops (D-048) |
| Bringing the lane to the new state | 941 ms | 0.5 ms | `Tree` and `SymbolTable` are persistent maps: the copies of the state cost nothing (D-052); the lane's state is not saved here |

The parse cache went from 64 to 1,024 entries in the same work. On 5,000
files, same build except for the constant:

| Integrating | 64 entries | 1,024 entries |
| --- | --- | --- |
| 100 ops (50 files) | 71–96 ms | 37–38 ms |
| 1,000 ops (500 files) | 1,034–1,092 ms | 316–318 ms |

A save in a lane or in the main directory costs what it did: the phase 2
code and this one, run one after the other on the same machine, both process
a save of the ordinary 2,000-line file in 12–15 ms at p50 and 19–21 ms at
p95.

### Creating a lane

```bash
cargo run --release -p kito-lane --example lane_creation -- --files 50000
```

No file system with reflinks was available on the test machine, so the
figures are for the fallback: every file copied. Per file, the first version
tried to clone, failed, removed what the attempt left and copied (about 20
system calls); now the first failure settles the matter for the whole lane,
and a small file is one read and one write (10 system calls).

These figures vary more than the others. On this VM (ext4 mounted with
`discard`, block bitmaps read on first use) creating a file costs from 13 µs
to 250 µs depending on what the file system has in memory, and the same
50,000-file lane takes from 0.5 s to 2 s, sometimes more, on consecutive
runs of the same binary. The "after" above is a quiet run.

## Phase 3 · the git commit of every integration

```bash
cargo run --release -p kitod --example scale -- --files 50000 --git-export
```

With `export_on_checkpoint` on, which is the default, every integration also
writes a git commit. The export replayed the main lane's log from its first
op to know the files at the checkpoint: on 50,000 files (250,000 ops) that
was a second per commit. The daemon now remembers the files at the last
checkpoint it exported, and the commit is made from what differs.

| On 50,000 files, git export on | Before | After |
| --- | --- | --- |
| Integrating 100 ops | 1,097 ms | 135–145 ms |
| Integrating 1,000 ops | 1,520 ms | 447–511 ms |
| Integrating 100 ops from a lane 550–650 files behind | 1,065–1,207 ms | 185–245 ms |

The last row includes rewriting those 600 files in the lane's working copy.
A test checks that the commit made this way is the one the log gives, bit
for bit.

## The first capture of a large project

```bash
cargo run --release -p kitod --example scale -- --files 50000
```

`kito init` in a directory that already has 50,000 files (or the first scan
after a long absence) records every file and every symbol in it.

| First capture | Before | After |
| --- | --- | --- |
| 5,000 files (25,000 ops) | 3.2 s | 1.3 s |
| 20,000 files (100,000 ops) | 20.1 s | 5.2 s |
| 50,000 files (250,000 ops) | 88–92 s | 14.7 s |

The time grew faster than the files: the step that pairs the old and the new
content of each file looked each path up by scanning the list of the paths
of the batch. With that gone, the rest was parsing, on one thread. Many new
files are now parsed on every processor, 256 at a time, so that the syntax
trees of a whole repository are never in memory together; nothing can have
moved between files that are all new, so the ops are the ones the files
would give one by one (a test compares the two).

Where the 14.7 s go: parsing 5.3 s (four cores), recording 250,000 ops
4.5 s, storing the text of 200,000 symbols 1.3 s, reading and storing the
files 0.8 s. `kito import` of a git repository with the same files in one
commit takes 30 s: it records the files from git's objects, analyzes them,
and then the daemon reads the working directory once.

## Compared with git

The same operations with git 2.43 and with Kito, on the same machine and
the same 50,000 files, one after the other (`git worktree` for the agents'
working copies, `git add -A && git commit` in the worktree and
`git merge --no-ff` for bringing their work in):

| Operation | git | Kito |
| --- | --- | --- |
| First import of the 50,000 files | 2.6 s | 14.7 s |
| Seeing that nothing changed (`git status` / a scan) | 48 ms | 55–60 ms |
| Recording one save | 154 ms (`git add <file> && git commit`) | 13–20 ms of work, about 70 ms after the save with the debounce; no command |
| A working copy for an agent | 1.6 s (1.56–2.0 s) | 1.1–3.1 s in the same run; 0.46–0.56 s on a quiet disk |
| Bringing in an agent's work, 50 files | 523–676 ms | 135–245 ms with the git commit, 86–206 ms without |
| Bringing in an agent's work, 500 files | 1,202 ms | 447–511 ms with the git commit |

git does not parse anything, which is the whole difference in the first row.
The comparison says nothing about what git cannot do at any speed: the two
agents of the phase 3 demo conflict in git and do not in Kito.

### The same merges, given to git

```
cargo run --release -p kitod --example swarm -- --agents 10 --rounds 4 --compare-git
```

The swarm of phase 3 and, after it, the same integrations through git. For
each one git is given what Kito merged: the state the lane started from,
the main lane as it was, and the lane's working copy, through
`git merge-tree --write-tree`, the merge of `git merge` without a working
tree. Where git stops, the work goes on from Kito's result, as if someone
had settled the conflict that way.

| The same work | git | Kito |
| --- | --- | --- |
| 10 agents, 4 rounds: 40 integrations | 4 merged, 36 conflicts | 40 merged, 0 conflicts |
| 6 agents, 8 rounds: 48 integrations | 8 merged, 40 conflicts | 48 merged, 0 conflicts |

Where git merged, its files are the ones Kito wrote (12 of 12). Every
conflict of git is in `src/shared/registry.ts`, the file where each agent
adds a method and a function at the same place; the rest of the script (a
function each in a shared file, the renames, the function that moves, the
one that is removed) git merges too. The script was written around that
case: the numbers say how far apart the two are on it, not how often real
work looks like it. No history of real merges has been replayed.

The same runs show what the queue costs: one integration at a time, each
with its type check, kept an agent waiting 11.6 s at the median with ten
agents and 7.1 s with six.

## Phase 4 · what a question to level 2 costs

An integration in which neither side renamed anything asks nothing: finding
that out is a comparison of the two symbol tables that looks only at what
differs. On 50,000 files, after this phase: 87 ms for 100 ops, 425 ms for
1,000 (the first version of the phase scanned the lane twice, 60 ms more;
the question is now made by the merge itself, after its one scan).

```bash
cargo run --release -p kitod --example scale -- --files 50000 --rename
```

| Who answers | Explicit rename in a lane | Integrating a lane that renamed |
| --- | --- | --- |
| The syntax alone, 5,000 files | 109 ms | 122 ms |
| The syntax alone, 50,000 files | 817–893 ms | 756–773 ms |

The syntax-only provider reads every file of the language to see whether
the name is written in it, and parses the ones where it is: that is the
whole cost, and it grows with the repository.

With `typescript-language-server` on a project of 41 files, an explicit
rename that updates 80 uses in 40 files takes 1,060 ms the first time (the
server starts and loads the project) and 224–240 ms afterwards, 150 ms of
which are the wait of D-063.

## Phase 5 · from a save to its op

```bash
cargo run --release -p kitod --example latency -- --shape typical
```

| File | Before (after 50 ms of quiet) | After (at the closing of the file) |
| --- | --- | --- |
| 2,000 lines, 143 functions (58 kB), p50 / p95 | 66.6 / 72.3 ms | 15.9 / 21.2 ms |
| 2,000 lines, 2,000 functions (172 kB), p50 / p95 | 84.7 / 103.5 ms | 35.3 / 53.2 ms |

Nothing in the processing changed (p95 19.9 ms and 51.5 ms): what went away
is the wait (D-067).

## Phase 5 · what a save costs in the store

```bash
scripts/store-growth.sh
```

One line of a 69 kB source file (`crates/kitod/src/repo.rs`) changed and
saved 200 times:

| | Before | After |
| --- | --- | --- |
| Growth of the blobs per save | 20,279 bytes | 910 bytes |
| The 200 saves | 4,055 kB | 182 kB |

A new version of a file is stored as a delta from the one before (D-069).
The latency of a save is the same with and without (p95 21.3 ms): the
previous version is in memory when the next one arrives.

## Phase 5 · integrations with their git commit

| On 50,000 files, git export on | Before | After |
| --- | --- | --- |
| Integrating 100 ops | 125–145 ms | 93–110 ms |
| Integrating 100 ops from a lane 550–650 files behind | 207–260 ms | 135–187 ms |

git's index is rewritten after the lane has its answer, and the files of
an integration are written by several threads (D-068). Step by step, for a
lane that is behind: scanning the lane 55–70 ms, merge 10 ms, writing and
recording the merged files 30 ms, the git commit 10 ms (37–44 ms with the
index), bringing the lane's 600 files up to date 15–20 ms (45–48 ms one at
a time).

## Phase 8 · a save, from one machine to another

`cargo test -p kito-server --test mirror a_follower -- --nocapture` writes
a file a hundred times in the working copy of a lane on a connected
directory, and times each from the moment the file is on that disk to the
moment the same content is on the disk of another connected directory
that follows the lane. Both directories and the server are on one machine
(loopback); the daemons are debug builds.

| | time |
| --- | --- |
| median | 19 ms |
| 95th percentile | 21 ms |
| slowest (the first) | 80 ms |

About 10 ms of each is deliberate: a file closed after being written is
given that long before it is read, so that the temporary files many
programs write beside the real one are not recorded as saves. The rest is
four trips to the server: the save, the event, the list of what changed,
the content.

With release builds, on repositories made of generated TypeScript files:

| | 2,000 files (17 MB) | 20,000 files (87 MB) |
| --- | --- | --- |
| `kito lane open`, files brought here | 1.3 s | 3.2 s |
| `kito sync <lane>`, nothing to send | 80 ms | 190 ms |
| `kito sync <lane>` after one save | 110 ms | 180 ms |
| `kito lanes` | 11 ms | 48 ms |

Before `ping` (D-091) every command paid for a whole `status` of the
repository on the server: `kito sync` took 265 ms and 2.2 s.

## Phase 10 · asking why

A repository of 20,000 generated TypeScript files with 480,000 functions,
recorded as 500,000 operations under one checkpoint; then a lane that
changes one function and is integrated. Release builds, one machine;
times are of the whole `kito` command.

| | time |
| --- | --- |
| The first `kito why` after the daemon starts (reads the log once) | 1.3 s |
| `kito why <file>:<function>` after that | 9–37 ms |
| `kito why <file>` (95 lines of answer) | 9 ms |
| `kito why <file>:<line>` | 9 ms |
| `kito why <file>:<function> --uses` (the syntax of 20,000 files read) | 0.3 s |
| `kito why <file>:<function> --lane <lane>` (the lane looked at first) | 0.1 s |
| `kito reasons <words>` | 4–6 ms |
| `kito why` right after an integration (the index is not rebuilt) | 19 ms |

Two things were measured and removed on the way. The first version held
every operation of the log in memory while it built the index; it now
reads them one at a time. And it looked at the whole main directory
before every question, which took 2 s here and tells nothing: what is
saved there is recorded as it is saved.

## Phase 11 · the rules, checked before an edit

The same repository of 20,000 files and 500,000 operations, with one lane
integrated and others open. Release builds, one machine; times are of the
whole command, as Claude Code runs it around a tool.

| | time |
| --- | --- |
| `kito hook before-edit`, a file of a lane, let through | 21–24 ms |
| `kito hook before-edit`, held for a reason | 22–25 ms |
| `kito hook before-edit`, a file of the main directory (refused) | 3 ms |
| `kito hook after-shell` (60 ms of it waiting for the save to be recorded) | 88–106 ms |
| `kito hook session-start` (the board of every open lane) | 100–130 ms |
| `kito lane open` on one function, with why it is the way it is | 0.4 s |
| `kito lane open` on a directory of 100 files | 0.7 s |
| `kito lane scope --add` of one function, with its why | 0.14 s |
| The first of these after the daemon starts (reads the log once) | +1.3 s |

Before an edit the daemon looks at the lane's working copy (what the hook
judges is the file as it is), then reads the history of one file. The cost
does not grow with the repository but with how often that one file
changed (at most 400 versions are read).
