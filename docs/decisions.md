# Decisions not covered by the plan

Choices made during implementation on points the project plan does not
settle. The plan's rule: the simplest and most reversible option, recorded
here. Decisions that change the semantics of a data model type go in an ADR
(`docs/adr/`) instead.

## Phase 0

### D-001 · Binary encoding: CBOR
On-disk records and the input of content hashes are in CBOR (RFC 8949,
`ciborium` crate). It is self-describing: structs are maps with the field
names. The plan says fields can be added to the data model; with CBOR a new
optional field, omitted when absent, does not break the reading of old
records and does not change the hashes already computed. A positional format
(postcard, bincode) would be more compact and faster but would turn every
added field into a migration. A typical op takes about 300 bytes.
*Reversible:* the encoding is isolated in `kito_core::codec`; the header of
every log file carries a format version number.

### D-002 · `OpId` covers only (parents, kind, author)
As per the plan: "hash of (parents, kind, payload, author)". `lane`, `at` and
`intent` stay out: they are metadata, and the same change by the same author
on the same parents is the same op. Structured hashes use BLAKE3 in
`derive_key` mode with one context per type, so an `OpId` cannot collide with
a `BlobId`. Blobs use plain BLAKE3 (it matches `b3sum`).

### D-003 · Types the plan names without defining
`LaneId(u64)` (0 = main lane), `CheckpointId(u64)` and `ConflictId(u64)` are
counters local to the repository; `Author { kind: Agent | Human, name }`;
`Hlc { wall_ms, logical }`; `Location { file, line, start, end }` with byte
offsets; `TextEdit { start, end, text }` with ranges that refer to the
original text; `Verification { commands, passed, duration_ms }`;
`Fingerprint { sketch: Vec<u32> }`, a MinHash sketch whose similarity is the
fraction of equal positions (the algorithm that computes it arrives with
phase 2).

### D-004 · UTF-8 paths
The `PathBuf`s of the data model are relative to the repository root and must
be valid UTF-8: a path that is not cannot be serialized. Files with non-UTF-8
names will be skipped with a warning by capture (phase 1).

### D-005 · A single global oplog, not one per lane
`oplog/` is a single segmented log; the `lane → op` index gives the per-lane
view. It is the plan's layout and makes the total recording order trivial.

### D-006 · What "confirmed" means
Writes are queued in memory; `Store::commit` makes them durable. An op is
confirmed when the `commit` that follows it returns `Ok`. The order inside
`commit` is mandatory: fsync of the packs, then write and fsync of the oplog,
then the index. The oplog buffer does not touch the file before the packs are
durable, so an op present on disk never references a missing blob, not even
after a power loss.

### D-007 · A torn tail and damaged confirmed data are different cases
On open, the active file of every log is reread in full. The first invalid
record and everything that follows it are discarded (after saving a copy in
`lost+found/`): this is the plan's "truncated final record". But the index
records how far the logs were already confirmed, and below that threshold
recovery discards nothing: it returns `StoreError::Corrupt`. Accepting the
loss requires opening with `StoreOptions::repair`. Sealed segments are never
truncated: a damaged record in one of them is an error on read and in
`Store::verify`. Without this distinction a single damaged bit in the middle
of the active segment would silently make all the later confirmed ops
disappear.
*Known limitation:* between one index sync and the next (1 second by
default) the threshold lags behind, and damage in that window is treated as a
torn tail.

### D-008 · Write error = poisoned store
After a write I/O error (disk full, failed fsync) the state of the files is
uncertain: the store rejects every further write with `Poisoned` and must be
reopened. Reads remain possible. This is simpler than attempting an in-memory
rollback, and recovery on open is already the well-tested path.

### D-009 · redb index with non-durable commits
The index is updated on every commit but fsyncs only every
`index_sync_interval` (1 s) and on close. After a crash it may lag behind the
logs: opening rereads the tail of the logs and catches it up. If it is
missing, unreadable, ahead of the logs or does not match them (the CRC of the
last indexed op is compared), it is rebuilt from scratch.

### D-010 · Size limits
A record carries at most 2 GiB − 1 of payload, so a blob cannot exceed that
size (large binary files are out of scope for v1). A blob larger than a pack
gets a pack of its own.

### D-011 · Readers in other processes: deferred
Reads go through `mmap` inside the process that owns the store. A reader in
another process (the CLI reading the files without going through the daemon)
is not provided for in this phase: the architecture routes CLI and MCP
through the daemon. If it becomes necessary, a shared lock that excludes
recovery's truncation will have to be added.

### D-012 · Crates are created with their phase
The workspace will reach 12 crates; each one is created when the phase that
fills it begins, instead of keeping empty crates around.

## Phase 1

### D-013 · File type (`FileMode`)
`FileAdd` has an optional `mode` field (regular, executable, symbolic link),
absent from the encoding when it is "regular". Without it, the git export
would lose the executable bit and CI would stop working. It is an added
field, not a change of semantics: the hashes of the existing `FileAdd`s do
not change. The model has no op for a type change alone, so a `chmod +x` is a
`FileRemove` followed by a `FileAdd`.

### D-014 · The tree is rebuilt from the ops alone
`Tree` (path → blob) is obtained by replaying the ops, without ever re-running
a parser: the same history gives the same tree even with future versions of
the grammars. Consequence for phase 2: symbolic ops will have to carry their
effect on the file as well (blob before and after), or be accompanied by an
op that does.

### D-015 · Inline edits only when small
`TextEdit.edits` contains the exact edits (line diff) when the file is text
and the change weighs less than 32 KiB; otherwise it is empty, and the change
remains described by the two blobs. This avoids writing large contents twice.

### D-016 · Exact renames only
A file that disappeared and one that appeared in the same group, with
identical (non-empty) content and the same type, are a `FileRename`. No
similarity heuristics: a false rename pollutes the history. A rename with a
modification in the same save becomes a removal plus an addition.

### D-017 · Git export
- Commits are deterministic: author from the checkpoint, date from the last
  op, message = label plus `Kito-Checkpoint: ckpt#N`. Email address
  `<author>@kito.invalid`, configurable.
- The first export into a git repository that already has commits takes them
  as parents: the history continues.
- The branch advances only by fast-forward. If it points to a commit Kito
  does not know, the export stops (`Diverged`). If it had fallen behind
  because of an interrupted export, it is realigned.
- After the export, git's index is rewritten from the latest tree.

### D-018 · Git import
The first-parent chain of HEAD is imported: every commit becomes the ops that
lead from its parent's tree to its own, plus a checkpoint that remembers the
commit (which is therefore not re-exported). Submodules and non-UTF-8 paths
are skipped and listed. `kito import` creates `.kito/` if it is missing.

### D-019 · Protocol between daemon and client
JSON-RPC 2.0, one message per line, over a Unix socket at `.kito/kitod.sock`
(in `/tmp` if the path exceeds the socket length limit). It is the default
indicated among the plan's open questions; a binary protocol remains possible
because the transport is isolated in `kitod::rpc` and `kitod::client`.

### D-020 · `sync` is a full scan
The barrier does not trust events: it compares the whole working directory
with the tree (with the stat cache it costs one `stat` per file). When it
returns, whatever was on disk is in the log, even if the watcher missed
events.

### D-021 · A tracked file stays tracked
As in git, an ignore rule added later does not make an already tracked file
disappear from the log; it applies to new files.

### D-022 · `kito init` starts the daemon and does not override git
Capture is continuous only if the daemon is running: `init` and `import`
start it. In a directory that is already a git repository `init` refuses and
points to `kito import` (which brings its history in) or `init --fresh`.

### D-023 · Commands beyond the plan
`kito sync` (the barrier, which the plan provides only as an MCP tool),
`kito status` and `kito daemon start|stop`.

### D-024 · Saved state of the main lane
`.kito/lanes/main/state` contains the tree and the stat cache, to restart
quickly. It is derived data: if it is missing, unreadable or does not match
the log, the ops are replayed.

### D-025 · When file events do not arrive
On some file systems the operating system does not report every change: the
typical case is `/mnt/c` under WSL, where what a Windows program changes
generates no events in Linux, but the same holds for network shares. Three
measures:

- `kito log` and `kito status` run the barrier (`sync`) before answering:
  what they show always reflects the disk, regardless of the watcher.
- If a scan finds changes while the watcher has been silent for more than a
  second, the daemon concludes that events are not enough there and switches
  to frequent scanning: at most one scan per second, and spending no more
  than a twentieth of the time on scans. It starts out this way directly on
  the file systems known for this problem (9p, drvfs, cifs, nfs…) and with
  `capture.watch = false`.
- `kito daemon status` says which interval is in use and why; `init` and
  `import` warn right away.

With scanning in place of events, capture stays complete but less
fine-grained: the saves between one scan and the next merge into a single op,
and the recorded time is that of the scan.

### D-026 · Identity
Who signs captured changes is resolved, in order, from: the repository
config (`[author]` in `.kito/config.toml`), the environment (`KITO_AUTHOR`,
`KITO_EMAIL`), the personal config (`~/.config/kito/config.toml`), git's
`user.name`/`user.email`, and finally `$USER`. Name and email are resolved
independently, as git does. `Author` gained an optional `email` field (absent
from the encoding when unset, so existing op IDs are unchanged); exported git
commits use it, which is what lets GitHub attribute them. `kito identity`
shows the identity in use and where it comes from, and sets it
(`--name`, `--email`, `--agent`/`--human`, `--global`).

### D-027 · Rollback never rewrites history
`kito rollback <point> [paths…]` brings the working directory (or only the
given paths) back to the state at a checkpoint, at an op, or N ops ago. The
log stays append-only: the current state is captured first, then the files
are rewritten and the difference is recorded as new ops carrying an intent
("rollback to ckpt#1"). A rollback can therefore always be undone by rolling
back to the previous head, which the command prints. An untracked (ignored)
file sitting where a file must be recreated is never overwritten: the
rollback refuses instead.

### D-028 · Intents live in their own append-only log
`.kito/intents` stores `Intent` records like `.kito/checkpoints` stores
checkpoints. An op may only cite an intent the store already knows, and
`Store::verify` reports ops whose intent is missing.

### D-029 · File-level ops carry the content, symbolic ops say what it meant
A save of a parsed file is recorded as the same `TextEdit`/`FileAdd`/… as in
phase 1, followed in the same batch by the symbolic ops. The tree of a lane
is rebuilt from the file-level ops alone, so replaying a log never needs a
parser or a grammar of the right version; the symbol table is rebuilt from
the symbolic ops alone. The plan's "`TextEdit` as a fallback for what cannot
be parsed" still holds: a file with no grammar, or too large (2 MiB), has
only that.

### D-030 · How a symbol is recognized
Thresholds, all in `kito-diff/src/ast.rs` and all conservative (a false
identity pollutes history and merges, a missed one costs an add and a
remove):

- same name and kind in the same scope → same symbol, whatever changed
  inside; among overloads, the unchanged ones pair up first;
- same body, other name → rename; a body under 8 tokens also needs the same
  signature and no twin among the new symbols;
- fingerprint similarity ≥ 0.8, or ≥ 0.5 with the same signature, for
  definitions of at least 12 tokens; smaller containers are judged by the
  names of their members (≥ 80% in common);
- across scopes and files (moves): same body of at least 8 tokens; or same
  name, body and signature in a scope of the same name (or in/out of a
  namespace); or same name and similarity ≥ 0.8.

The values were tuned on the corpus of phase 2 and are measured by
`cargo run -p kito-diff --example corpus`.

### D-031 · What is a symbol
Functions, methods, classes, structs, enums, interfaces/traits, type aliases,
modules/namespaces, and top-level constants and variables. Fields, enum
members, parameters and locals are not: they belong to the body of what
contains them. A function nested in a type is a method in every language.
Rust `impl` blocks are containers named after their type.

### D-032 · A symbol's hashes ignore its own name
Inside its own definition a symbol's name is hashed as a placeholder. A
recursive function, a parameter named like its function, a type alias that
mentions a value of the same name: renaming them changes their body text,
and without this rule a third of the missed renames on the corpus were of
this kind (TypeScript went from 80% to 100%).

### D-033 · Moves at any depth; `Move` carries the new enclosing symbol
A symbol can leave its container: a class cut from a namespace block into
another file, a method that goes to another class. `OpKind::Move` gained an
optional field, `parent`, the enclosing symbol after the move. It is absent
from the encoding when `None`, as in every op written before it existed
(those only moved top-level symbols), so stored ops keep their IDs. A moved
container still takes its members along without ops of their own. Kind is
part of identity: a method that becomes a free function is a new symbol.

### D-034 · Order of the symbolic ops of a batch
Additions (containers first), then moves (outermost destination first), then
renames and modifications, then removals (members first). Every op applies
to the table left by the previous one: a symbol always arrives where its new
container already is.

### D-035 · `kito why` reads more than the symbol's own ops
The story of a symbol is its own ops, plus the moves between files of the
symbols that contained it at the time, plus the renames of its file. File
renames are not indexed by symbol: they are read from the log of the lane
only when the symbol's own ops do not leave it where it is now, so a file
renamed and renamed back is not shown. An index of file renames can be added
if that reading shows up in a measurement.

### D-036 · Half-written code
A save that does not parse cleanly is recorded as text and leaves the
symbols alone; the daemon remembers, per file, the last content that parsed
and compares the next clean save against it. Typing in the middle of a
function therefore never removes and re-creates the symbols below it.

### D-037 · Files the history only knows as text
A repository imported from git, or captured before phase 2, has files with
no symbols. They are analyzed once (at import, and at the first start of a
daemon of this version) and get `AddSymbol` ops with no file-level op.
Symbol IDs start there: the history before that point stays textual.

### D-038 · Rollback and symbols
A rollback is recorded like any other change: new file-level ops, and the
symbolic ops the comparison gives. A symbol removed and brought back by a
rollback is a new symbol unless the same batch still has the old one to pair
with. `~N` and `--ops N` count file-level ops: the symbolic ops that follow a
change are part of it. `kito log -n` counts the same way.

### D-039 · Incremental parsing and the parse cache
The daemon keeps the syntax trees of the last 64 contents it parsed (16 MiB
at most). The old side of a save is almost always there, and tree-sitter
reparses the new side from it. Compiled queries are shared by all threads.
Measured in [`benchmarks.md`](benchmarks.md).

