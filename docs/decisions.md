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
Symbol IDs start there: the history before that point stays textual. In
`kito log` those ops get a line of their own ("Symbols"), since they belong
to no file change; in the JSON output a symbolic op names the change it was
recorded with (`change`), and these have none.

### D-038 · Rollback and symbols
A rollback is recorded like any other change: new file-level ops, and the
symbolic ops the comparison gives. A symbol removed and brought back by a
rollback is a new symbol unless the same batch still has the old one to pair
with. `~N` and `--ops N` count file-level ops: the symbolic ops that follow a
change are part of it. `kito log -n` counts the same way.

### D-039 · Incremental parsing and the parse cache
The daemon keeps the syntax trees of the last contents it parsed: 1,024 of
them, 16 MiB of source at most (64 until phase 3: an integration reads many
files at once, see D-052). The old side of a save is almost always there,
and tree-sitter reparses the new side from it. Compiled queries are shared
by all threads. Measured in [`benchmarks.md`](benchmarks.md).

## Phase 3

### D-040 · Where a lane starts
A lane starts from the main lane as it is when the lane is opened, saves not
yet under a checkpoint included; `--base` starts it from a checkpoint
instead. The `base` the data model asks for is the latest checkpoint; if the
repository has none yet, one named `start` is made. The exact starting point
is `base_head`, the op the main lane was at.

### D-041 · What the store knows of a lane
`Lane` gets four fields (ADR not needed: fields are added, none changes
meaning): `base_head`, `since` (how many of its ops were already integrated),
`task` and `next_symbol`. The records are in an append-only log of their own,
`.kito/lanes/journal`, last one wins. The state of a lane is always the main
lane at `base_head` plus the lane's ops from `since` on: it can be rebuilt
from the logs alone, and a working copy that is lost is made again. Symbols a
lane creates get IDs with the lane as origin; `next_symbol` makes sure a
number is never used twice, even after the lane restarts from a new base.

### D-042 · An integration is a squash
What reaches the main lane is the merged result, recorded as new ops signed
by the lane's agent: the first has two parents, the head of the main lane and
the head of the lane, so the graph says where it comes from. The lane's own
ops stay in the lane's log. Replaying them one by one on the main lane was
the alternative: it gives the same files and a longer, noisier log, and every
intermediate state would have to be verified or trusted unverified. Symbols
keep the IDs the lane gave them: the merge tells the capture which symbol is
at which place of the merged files (`Identities`, D-030). Every integration
makes a checkpoint, and so a git commit, authored by the agent. Afterwards
the lane is brought to the new state of the main lane and keeps its ID.

### D-043 · How files are merged
A file is a header, units and a tail; a unit is a symbol with the text that
leads to it (comments, decorators, `export`), and whole lines only. A symbol
that contains others is merged the same way, one level down. Consequences:

- Units added by the two sides at the same place are both kept, ordered by
  symbol ID: the result does not depend on who integrates first.
- The order of the units is the main lane's; a side that only reorders, in a
  file the other side changed too, loses its order (a reordering is not an
  op).
- A symbol one side moved is merged where it went, with its indentation
  taken off before the merge and put back after.
- A symbol whose kind changes is a new symbol (D-033), here too.
- What cannot be read this way — no grammar, a syntax error on either side,
  symbols the history does not know, more than 2 MiB — is merged as text,
  line by line, and so are the header and the tail.
- A file one side renamed without changing it follows the rename with the
  other side's content.

Only the files the lane touched are read, and those tied to them by a symbol
that crossed files or by a rename on the main lane: a lane that stayed
behind does not pay for what only the main lane changed.

### D-044 · Conflicts
A conflict is recorded in its own append-only log (`.kito/conflicts`) with
the three texts as blobs, the symbol or file it is about, and the lane's
agent as `assigned_to`. It blocks that lane only. A decision — the main
lane's text, the lane's, or a new one — is recorded on the conflict and
applied by the next integration of the lane, if the texts it was taken on
are still the ones in conflict; otherwise the conflict stays open, with the
texts as they are now and no decision. A conflict whose cause went away is
closed. `Conflict` gets the fields for this
(`file`, `name`, the three texts, `resolution`): added, none changed.
Conflict markers are never written in a file.

### D-045 · Verification
The commands of `[verify] commands` run with `sh -c`, in order, in a copy of
the merged result (`.kito/verify/work`, made like a lane's working copy and
with the same shared links), with `KITO_VERIFY=1` and nothing on standard
input. The first that fails stops the integration; the last lines of its
output go in the report. A command that runs longer than `timeout_secs`
(600 by default) is killed with every process it started. The repository is
not held meanwhile; if the main lane or the lane moved in the files involved,
the integration is prepared and verified again (three times at most). With
no commands configured, nothing is verified: the plan's verifier is whatever
the project uses, and Kito cannot guess it.

### D-046 · The integration queue
One integration at a time, in the order of the requests: the result of each
is the starting point of the next. A request returns a job; the daemon keeps
the results of the last 256 jobs for whoever asks. `kito integrate lane/1
lane/2` asks and waits, lane after lane.

### D-047 · Files caught in the middle of a save
Most programs save by emptying the file and writing it again. A capture that
reads in between sees an empty or half-written file, records every symbol as
removed and then, a moment later, as new: new IDs, and a fake conflict with
whoever changed those symbols elsewhere. The swarm simulator found this in
the first runs. Three defenses, in the order they act:

- a file modified less than 25 ms ago is not trusted yet: the capture waits,
  or, for file events, leaves it for the next round;
- on Linux, where the system says when a file is closed, a file that is
  open for writing is left alone until it is closed;
- a symbol removed and seen again in the same file, with the same enclosing
  symbol, kind and name, gets its ID back. `kito why` says "restored".

The third is the one that holds when the first two are fooled (a slow disk,
events that arrive late), and it makes a rollback bring back the same symbol
too, which D-038 did not.

### D-048 · What an integration looks at, and when states are saved
An integration scans the lane's working copy whole, and of the main
directory only the files it is going to merge or write, before merging and
again before writing: a save not yet recorded there is never written over.
The rest of the main directory is the watcher's business. The saved states
(`state` files) are written when the daemon stops and at checkpoints made by
hand, not at every integration: they are caches, checked against the log
when the repository is opened. After a crash the lanes are rebuilt from the
log and reread their files once.

### D-049 · Explicit renames
`kito rename` (and `kito.rename_symbol`) writes the new name in the
definition and in the uses inside the symbol itself, and records a `Rename`
op directly. The other places where the old name appears as a word are
returned as a list, not edited: knowing which of them refer to this symbol
is level 2 (phase 4).

### D-050 · The MCP server
`kito-mcp` is a separate process that speaks MCP on standard input and
output (the `rmcp` crate) and passes every call to the daemon over its
socket, starting it if needed. The tools have the names of the plan, with a
dot: `kito.open_lane`. For a client that does not accept dots in tool names
there is `--tool-prefix` (or `KITO_MCP_TOOL_PREFIX`): `--tool-prefix kito_`.
The command line prints the same structures with `--json`.

### D-051 · What a lane shares with the main directory
A working copy has the tracked files. What is ignored and the tools need
(`node_modules`, `.venv`, build caches) is not copied: the paths listed in
`[lanes] share` are linked. A lane that must change one of them is not
served by this.

### D-052 · Persistent maps for the state
`Tree` and `SymbolTable` keep their entries in persistent ordered maps (the
`imbl` crate): a copy shares everything with the original. A lane's state
starts as a copy of the main lane's and is replaced by a new copy at every
integration, and the base is a third; with ordinary maps those copies were
most of the time of an integration on a large repository. The encoding does
not change (a map, keys in order). Measured in
[`benchmarks.md`](benchmarks.md), together with the parse cache.

### D-053 · `kito why` across lanes
The story of a symbol is read from the lane asked for and from what that
lane is built on: the main lane up to the lane's base. Steps taken on a lane
say so ("on lane/3"); the op that brought a lane's symbol to the main lane
reads "integrated into the main lane".

### D-054 · Debug information of the dependencies
The development profile builds the dependencies without debug information:
with it, `target/` passed 25 GB. The workspace's own crates keep theirs.

### D-055 · The export remembers the files of the last commit
The export to git needs the files at the checkpoint and the paths that
changed since the commit before. From the log alone that is a replay from
the first op. The daemon keeps the tree of the last checkpoint it exported
(a persistent map: it costs nothing) and passes it to the export with the
current one; the commit is made from the difference. A daemon that has just
started, or an export of several checkpoints, reads the log as before.

### D-056 · Many new files at once
A batch of more than 256 new files in which nothing is known in advance and
nothing can be coming back (an import, the first capture of a project) is
analyzed 256 files at a time, parsed on every processor. Between files that
are all new no symbol can have moved, so the result is the one the files
would give one by one, IDs included. The syntax trees of these files are
not kept: the parse cache is for files being edited.

### D-057 · Debug information, again
The development profile keeps only line tables for the workspace's crates
and builds without incremental caches (D-054 was not enough: every test
binary links everything, and `target/` passed 20 GB a second time).

## Phase 4

### D-058 · What a provider answers with
The plan's trait returns `Reference`s, which go from a symbol to a symbol.
A use of a symbol is not always inside one: an import, a statement at the
top of a file. So "who uses this symbol" returns `Usage`s (file, bytes,
confidence, source), and a `Reference` is made from a usage where there is
a symbol to make it from. The data model is untouched. A language server
is as sure as it gets (1.0); the heuristic gives 0.8 to a plain name in the
file that defines it and to an import that leads there, 0.7 to the uses of
an imported name and to a member used inside its own type, 0.6 where the
file has namesakes or the module is matched by the end of its path, 0.5 to
a member used elsewhere. The threshold to act without asking is the
plan's: 0.9.

### D-059 · Level 2 is asked when it is needed
The plan describes level 2 as a background task that enriches an index
with the references between files. Nothing here needs every reference of
the repository: what is needed is who uses one symbol, when it has been
renamed. So there is no index: the providers are asked at that moment,
and nothing is stored. An index is needed to answer fast on a repository
where no language server is running, and belongs with the SCIP import of
phase 5. `references` (what leaves the symbols of a file) is implemented
by both providers and used by nothing yet.

### D-060 · The three states agree on the name before they are merged
The rule of the plan — a rename on the main lane and a new use on the lane:
the use is rewritten — was first done by rewriting the uses on the side
that had not renamed. A line that side had changed, and the other only
renamed a symbol in, then conflicted: two different changes to one line.
Now the uses the provider is sure of are rewritten on copies of all three
states: at the common starting point and on the side that did not rename,
where the provider finds them; on the side that renamed, where a line of
the starting point with a use is still exactly as it was (the provider
cannot be asked there: the old name no longer means anything). The merge
sees three states in which the rename has already happened everywhere, and
only what each side really did is left to compose. A use the author of a
rename forgot is renamed by the same means.

### D-061 · `semantic_ref` conflicts
A doubtful use is asked about only if it is new on its side (its line was
not in the file at the starting point): the ones that were there are the
renamer's business, and asking about every old suspect would bury the real
question. The conflict is about the renamed symbol; its three texts are the
old name, `renamed: a → b` on the side that renamed, and the list of the
suspected uses on the other. Its decision is one of the two the model
already has: taking the side that renamed means rewriting the uses, taking
the other means keeping them, and `--rewrite` and `--keep` are those two
said in a way that does not need to know which side is which. A new text is
refused. While it is undecided the same line may also raise an ordinary
conflict, since without a provider that is sure the states are not made to
agree.

### D-062 · Language servers
One server per language, in the main working directory, started at the
first question and kept. A state that is not on disk there (a lane, the
starting point of a merge) is shown as open documents with their whole
text, a file the state does not have as an empty document, and all of them
are closed after the question: the server is back to the disk, and no
question depends on the one before. A server is driven by a thread of its
own with blocking I/O: the provider does not depend on which async runtime
calls it, and the daemon and the library both use it. A server that fails
or passes its time limit is killed and started again by the next question;
one whose program is not installed is found out once and left alone. In
each case the heuristic answers and the result says what happened.

### D-063 · A server is asked when it has finished loading
A server asked right after it starts answers with what it has loaded: the
TypeScript server returned the uses in the open file only. The client says
it follows progress reports, and before a question it waits until the
server has no work in progress, giving it a moment to say it began: up to
3 seconds for the first question after a start, 150 ms afterwards, never
more than half of the question's time limit.

### D-064 · No `lsp-types`
The plan lists `lsp-types` among the dependencies of this crate. The
client sends seven kinds of message and reads places from two answers:
they are written with `serde_json`, and a crate of some thousand types was
not added for them.

### D-065 · Asking does not hold the repository
A question to a language server takes from a fraction of a second to
minutes. A merge that finds a rename returns the question instead of a
result; it is asked with the repository free, so that saves keep being
recorded; the merge is then prepared again with the answers, which are
used only if both lanes are where they were when the question was made.
An explicit rename does the same.

### D-066 · Tests of real servers
The tests that need `typescript-language-server` or `gopls` run where they
are installed and say so when they are skipped; with
`KITO_TEST_REQUIRE_SERVERS=1` a missing server is a failure. They were
first written to skip in silence, and for a while they did nothing: the
TypeScript on the path had no `tsserver`, and `gopls` was probed with a
flag it does not have.

## Phase 5

### D-067 · A finished save is not kept waiting
The debounce exists because an event does not say whether a save is over.
Some events do: on Linux the system reports a file closed after being
written, and a rename or a removal are over when they are reported. For
those the watcher records what the file system says of the file at that
moment; when every file of a batch has had such an event, the capture
starts after 2 ms of quiet instead of the debounce (enough for the rename
that follows a save to a temporary file to arrive with it). A file is then
taken at once if it is still as it was when the event was made, and goes
through the usual waiting if it has changed since. Events that say nothing
of the kind (a file created, written to and not closed, a new directory)
wait for quiet as before, and so does everything on systems that do not
report the closing of a file. The plan asks for a debounce of 50 ms and
for less than 30 ms from a save to its op: this is how both hold.

### D-068 · git's index is rewritten after the lane has its answer
Exporting a checkpoint ends by rewriting git's index from the commit, so
that `git status` is clean: it reads every tree of the commit and writes
every entry, 30 ms on 50,000 files. An integration makes the commit and
moves the branch, and leaves the index for when the lane has had its
result: the daemon rewrites it right after, without holding the
repository; closing the repository, a checkpoint made by hand and
`kito export-git` rewrite it too. For those 30 ms `git status` may show
the files of the last integration as modified; nothing in Kito reads the
index. The files an integration writes, in the main directory and in the
lane it brings up to date, are written by several threads.

### D-069 · A file saved again is stored as what changed
Every save put the whole file in the store, compressed: 20 kB for one line
changed in a file of 69 kB. A blob can now be stored as a delta: its
content compressed with another blob as dictionary, and the ID of that
blob. The capture, an integration and the git import say which blob a new
content is a version of (the file as the lane had it), and the store keeps
the delta if it is smaller. The op log is not touched and nothing is
rewritten: the log stays append-only, the IDs are those of the contents,
and reading a blob gives the same bytes, verified against its hash, however
it is stored. A chain of deltas is at most 24 long, then a version is
stored whole again; the last blobs read or written are kept in memory, so
that a save does not walk the chain. The plan asks for compaction between
checkpoints against the growth of the store: this removes most of that
growth as it happens, without a pass that rewrites history. The store's
format is now `kito-store 2`: an older store is opened and marked, and an
older Kito refuses a marked one rather than misread it.

### D-070 · More language servers, and what they taught
The provider was tried with the servers of eight languages (D-062 had two).
Three things came out of it, all in the provider and none in the servers'
favour or against: a server may refuse a request because what it knows
changed while it was answering (rust-analyzer, while it loads), and must
be asked again; a C or C++ server finds the uses in the files a compilation
database names, so a project without one gets the uses of the open file
only; and a server that does not say when it has loaded (pyright) is given
the three seconds of D-063 on its first question.

### D-071 · The SCIP index
`kito scip <index.scip>` imports an index made by an indexer
(`scip-typescript index`, `scip-java`, `rust-analyzer scip` and the like)
over the files of the main directory as they are at that moment. It is kept
as it was given, in `.kito/scip/`, with the content each of its files had
then. It answers "who uses this symbol" for the files that are still as
they were, as sure as a language server; for a file that changed since, and
for a symbol whose defining file changed, the syntax answers instead. So an
index never says anything false about the present, and goes stale one file
at a time: importing a new one now and then brings it back. A language
server, where one runs, is asked first. Of the format only documents,
occurrences, ranges (in the three ways the format has had), symbols and the
definition role are read; the messages are written by hand for `prost`, so
that building Kito needs no protobuf compiler. This is the index D-059
left for later, in the form the plan gives it: built in one go by a tool
made for that, not maintained by the daemon in the background.

### D-072 · `kito init` sets the project up for Claude Code
Telling Claude Code to work through Kito took four things done by hand: the
MCP server in `.mcp.json`, the permission to call its tools in
`.claude/settings.json`, the rules in `CLAUDE.md`, and `.kito/` in
`.gitignore`. `kito init` and `kito import` now do them, and `kito claude`
does them in a repository that already exists. It is the default because a
project is put under Kito to have agents work in it, and one that is not
ready for them does nothing wrong, only nothing: `--no-claude`, or
`KITO_NO_CLAUDE` in the environment, leaves the project as it is. The files
are the project's own, shared through git like the rest, which is why the
server is named `kito-mcp` without a path: it has to be on the `PATH` of
whoever uses it, and the command says so when it is not. Each file is only
added to: a server called `kito` that is already there is kept whatever it
runs, the keys of a JSON file stay in the order they were written in, and a
file that cannot be read as expected is left alone and named. The approval
of a project's server and the trust in the folder are Claude Code's
questions to the person, and are left to be asked: nothing is written that
answers them. What was tried: Claude Code 2.1 with the files as `init`
writes them connects to the server, opens a lane, renames, integrates and
closes; the rule in `.claude/settings.json` is read, and applies once the
folder is trusted. The rules say two things the trials taught: a rename
without a language server leaves uses to update by hand, and an integration
asked in the same step as an edit can go ahead without it.

### D-073 · `kito log` is short by default
Every save is an op, and the log gave each one a line, and under it a line
for every symbol it touched: a function worked on for ten minutes filled the
screen with itself. The log now has one line for a run of saves: the saves
of one file that follow one another, by the same author and for the same
reason, with no checkpoint between them. The line carries the ID and the
time of the last save (the point `kito rollback` returns to), the lines
added and removed in all, how many saves, and what they meant for the
symbols, each symbol once: a file created and then worked on is `new`, a
function changed six times is `changed`, one that came and went is not
named. Runs, not files: two files edited in turn are lines in turn, so the
order things happened in is still the order of the lines. `-n` counts
lines. `kito log -v` is the log as it was, and `--json` and the MCP tool
give the ops as before, to which each symbolic op adds the symbol's ID, its
name and, for a rename, the name it had. The folding is the command line's:
the daemon is asked for more ops until the lines wanted are whole.
The reason of a line is left out when it only repeats the label of the
checkpoint it is under, as the lines of an integration do.
The demos recorded for each phase show the log as it was then.

### D-074 · A scope is declared, and only grows
A lane says what it will change before it changes it: that is what lets
another lane, or a person, know it is there before the code shows it. A
scope is a list of functions and classes (by file and name, so that one
that does not exist yet can be named), files and directories; the empty
directory is the whole repository. It is kept on the lane's record, a new
field (`scope`, absent when empty, so older records read as they were),
each entry with the moment it was declared: the history of what a lane
said is on the lane. Entries are only added: narrowing a scope would let a
lane un-say what others were told. It is mandatory for every lane, the
ones people open included, and with it an intent, one sentence saying why,
which becomes the reason of what the lane records from its first save. A
symbol declared is remembered by its ID when it exists, so that a rename
does not take it out of the scope.

### D-075 · Work outside the scope is recorded, and stops the integration
The daemon cannot refuse a write to a working copy, and dropping one would
break the rule that nothing saved is lost. So a change outside the scope
is recorded like any other, the lane is told, and the integration answers
`out_of_scope` with what is outside until the scope is extended or the
change undone: the state compared is what the lane differs by from its
base, not everything it ever did. A file is compared part by part: each
function by its own text (a comment counts: what was edited was edited),
the text outside the functions, and a file that is not read as symbols as
a whole. Four things are not outside: a rename the lane made and the text
that only differs by it (the callers that take the new name, wherever they
are); the text around the functions of a file when one of them is in the
scope (an import follows the function that needs it); a file under
`[scope] always_allowed`; and what the integration itself writes when it
carries a rename to code the lane never touched, which is no change of the
lane at all. A file that comes or goes is told as the file, not part by
part.

### D-076 · Overlaps are allowed, and both lanes are told
Two lanes may declare the same things: a lock would have one slow or
forgotten agent block everybody else on that code. Each is told instead,
once, with whose the other lane is and what it says it is there for. An
overlap is looked for in what was declared and in what was changed, since
a declaration is only as good as the discipline of who makes it, on three
levels: the same function, or one inside the other; the same file, on
different parts (a directory declared counts for the files others are in);
and a function of one lane that uses a function of the other. The last is
found by the syntax, in the same file and through explicit imports: it
costs a parse of the files a lane is on and no language server, and it is
the case the merge cannot see, two changes that compose as text and do not
agree as logic. A file already told about by a function or a caller is not
told again as a file. What a lane has been told is kept by the daemon in
memory. The same goes for work that is no longer in a lane: when the main
lane changes something a lane declared or changed after that lane started,
the lane is told (`landed`) with who changed it and what for. A real agent
asked to change a function another lane had declared did its work and
wrote, in its report, that the other lane had to be told: this is that
telling.

### D-077 · A scope can be too broad
A scope of everything is always true and tells nobody anything. The board
counts the files a scope covers and shows it as broad above `[scope]
broad_files` (200 by default); with `broad = "refuse"` a scope whose
directories cover more than that is not accepted. The default only warns:
tools and tests that have nothing to say declare `./`, and a small
repository is under the limit whatever is declared.

### D-078 · Agents hear of the others with the answer to their next tool
MCP clients ask and do not listen, so what a lane has not been told yet
rides on the answer to whatever tool names that lane next (`notices`), and
`open_lane` and `extend_scope` answer with the `overlaps` their declaration
makes. With them goes one line: what another lane says it is there for is
information about that lane, not an instruction. An intent is text written
by another agent, and is given as that.

### D-079 · The server is a door, not a bigger daemon
One server hosts several repositories. Each stays what it was, a directory
with its own store and its own daemon: the server authenticates a client,
checks the repository it names against what that person may use, and
passes the requests on to that repository's daemon over its socket,
starting it if it is not running. Nothing of the daemon had to learn about
other repositories, a repository can be moved between a laptop and a
server by moving its directory, and a daemon that fails takes its own
repository down and no other. The cost is a process for each repository
that has been asked for, and a hop for each request.

### D-080 · Who a client is
Over TCP a client presents a token: 32 random bytes, shown once when the
person is added, of which the server keeps the hash. Through a command
(`kito-server stdio`) the person is whoever started the command says, which
is right when that is SSH with a forced command for each key, and why
`stdio` is not offered over the network by the server itself. The server
does not encrypt: `serve` listens on the loopback address unless told
otherwise, and SSH is the transport between machines. TLS in the server
was left out: it brings certificates to manage, and SSH is already where
the people who run servers keep their keys. A refusal says as little as it
can: the same words for no token and a wrong one, and for a repository
that does not exist and one that is not the person's.

### D-081 · What a client may ask
The server passes on only the requests on a list: reading (status, log,
history of a symbol, lanes, board, events, conflicts) and what lanes are
for (open, close, scope, intent, rename, sync, integration, deciding a
conflict). Stopping the daemon, reloading it, checkpoints and rollbacks of
the main lane, exports and imports take a path on the server or change
what everybody starts from, and stay with whoever runs it. On every
request it passes, the server writes who is asking, over whatever the
client put there: a lane opened through it belongs to the authenticated
person (`owner`, a new field of the lane, absent on older records).

### D-082 · Events are asked for, not pushed
The daemon keeps the last few thousand things that happened, numbered, and
answers "what came after number N" by waiting until there is something or
the wait runs out. It is the same request and answer as everything else,
so it needed no second protocol and passes through the server unchanged;
a client that watches keeps a connection for it. A save is told wherever
it was recorded (by the watcher, or by the scan an integration makes
first): the recording itself leaves the ops for whoever tells.

### D-083 · A connected directory
`kito connect` writes one file, `.kito/remote.toml`: where the server is
(an address and a token, or the command that reaches it) and which
repository. A directory with that file is found like a repository is, and
the client that would have looked for a socket connects to the server
instead: the command line and the MCP server did not have to know. The
file holds the token and is readable by its owner only. It is also where
the files of a lane will be when lanes can be worked on from another
machine.

### D-084 · A connected directory has a daemon of its own
The simplest way to let `kito` and `kito-mcp` work in a connected directory
was to give it what a repository has: a daemon on a Unix socket. `kitod`
started where `.kito/remote.toml` is serves the directory instead of a
store: it forwards requests to the server and keeps the working copies of
lanes (D-086, D-088). The clients no longer open the connection to the
server themselves, which also means one place holds the connections, the
journal and the watchers. The alternative, every command talking to the
server and a separate program syncing files, would have had two things to
keep in agreement.

### D-085 · The main lane's files are the connected directory
An agent started in an empty directory cannot read the code it is asked to
change, nor declare a scope. `kito connect` therefore brings the main lane
into the directory itself and follows it, as a clone that keeps itself up
to date. It is read only in effect: nothing saved there is sent (the main
lane is not written from elsewhere, D-081), and nothing changed there is
overwritten; `kito daemon status` lists such files. Mirroring the main
lane somewhere under `.kito` was the first version, and no agent would
have looked there.

### D-086 · The server's working copy stays what a lane is made of
A lane's files are still a working copy on the server, where integration,
verification and renames already work on them. A machine that writes
sends each saved file whole, with the ID of the content it started from
(`put_file`, `remove_file`). The server compares that with what the lane
has: the same, and the file is written as sent; different, and the two are
merged as text (three ways, from the content the writer started from);
not mergeable, and nothing is changed. Before comparing, the server
records what is on its own disk for that file, so that a change made
there is never written over. Sending ops instead of files would have
needed the parsing and the symbol IDs on every client, and two places
that could disagree about them.

### D-087 · A journal on the writer's machine
A save is copied into a journal (`.kito/lanes/<n>/journal`, content first,
then the entry, each made durable and put in place by a rename) before it
is sent, and removed when the server has answered. What both sides agree
on is the manifest: for each file, the content last known to be the same
here and there, kept as a snapshot and a log of changes. A daemon killed
at any point starts from those: entries are sent again in order (the
server takes a save it already has as done), and a file that differs from
the manifest with no entry gets one. Without the journal the last state of
each file would still arrive; with it every save does, in order, also
after a time with no server.

### D-088 · Followers fetch what an event names
A `saved` event carries the paths that changed (up to 500; none means
"look at everything"). A follower asks for those files and their
contents, and writes them unless the file here was changed since it was
last in step: then it is left, and shown. After a lost connection, or
events that were not kept, a follower compares every file. A writer
follows too: that is how a merge or a rename made on the server reaches
the machine that writes.

### D-089 · One writing machine for a lane, taken over explicitly
The server remembers which copy writes in a lane: the first to send a
save, or the last to claim it (`kito lane attach --write`). A save from
any other copy is refused with the name of the machine that has the lane,
and that copy turns into a follower that keeps its unsent saves. Only the
lane's owner may claim it. Two copies writing at once would have worked
most of the time through the merge of D-086, and failed in ways hard to
explain; several writers on purpose is a later phase.

### D-090 · The barrier refuses instead of integrating less
`sync`, `request_integration` and `rename_symbol` first send everything
saved here. If a save did not get to the server, or a file does not
merge, the request is answered with that, and is not passed on: an
integration that silently lacks the agent's last edit is worse than one
that waits. Requests that only read (`log`, `lane_status`) send what they
can and go on.

### D-091 · Small things found on the way
`ping` asks a daemon whether it is there; `status`, used for that until
now, costs in proportion to the repository and was paid by every new
session of the server. The client daemon keeps up to four connections to
the server for the next command. A connection found dead is replaced and
the request sent again only for requests that can be sent twice; before
one that cannot, a connection idle for more than a second is tried with a
request that costs nothing. The server reads its configuration again for
each new connection, so a person added while it runs is known at once.

### D-092 · A protected main lane puts edits back, and keeps them
"Refused" has to mean something for a file somebody already saved. The
choices were: make the files read-only (stops honest programs at the
source, but not root, nor anything that changes permissions, and every
integration would have to lift it); leave the edit and not record it (the
directory then differs from the main lane, and nobody knows which is
true); put the file back. Kito puts it back: the main directory is the
main lane again within milliseconds, and the edit is written first to
`.kito/refused/<when>/`, so nothing typed is lost. It happens where saves
are captured, so every way a save is noticed (events, scans, the look
before a merge) is covered by one rule. Off by default in a repository
made with `kito init`, where editing the main directory by hand is how a
person works alone; on for a repository a server adds with its history.

### D-093 · What still changes a protected main lane
An integration, which opens it for the moment it writes the merged
result. What Kito works out by itself from files already there (their
symbols) is not a change to them and is recorded as before. Turning the
protection on puts what the directory holds in a checkpoint first, if
files changed since the last one: the last state put there by hand has a
name in the history. Turning it off is a request only the repository's
machine can make, and is an event.

### D-094 · Three roles, kept by the server, applied in two places
`read`, `write`, `integrate`, each including the one before. A person has
one, and may have another in a named repository. The default is
`integrate`: what everybody could do before roles existed, so that a
server set up with 0.8 behaves the same. What a reader may ask is a list
at the server's door, as the list of what any client may ask already was
(D-081); who may integrate and whose lane it is are decided by the
repository's daemon, which gets the role with every request beside the
person (`on_behalf_of`). A repository with no server in front has no
roles: whoever reaches its socket runs it.

### D-095 · Asking to integrate without the right to
The request is not refused: the lane is merged with the main lane and
the checks run, so that its owner hears of conflicts and failing checks
at once, when they can fix them. Only the last step is held. The lane
records who asked and how many operations it had (`proposed`, a field
added to the lane; no type changes meaning, so no ADR). With more
operations than that the approval is stale and the lane shows as active
again. Approving is integrating: the person who may does `kito integrate
<lane>`, and the merge and the checks run again on the main lane as it is
by then. A separate "approve" that reused the earlier check would have let
in a lane checked against a main lane that no longer exists.

### D-096 · The rules of a repository are set through its daemon
Protection, checks and the git remote live in the repository's
`config.toml`. `kito-server repo set` changes them by asking the
repository's daemon (`protect`, `configure`), which writes the file and
applies them from the next request, with no restart; neither request is
on the list of what a client may ask. `repo list` reads the files.

### D-097 · The push is `git push`, after the answer, never forced
gix does not push yet, so the daemon runs `git push <remote> <branch>`,
with no terminal to ask a password on and two minutes to finish. It runs
after the lane has its answer: a slow remote delays nobody. A failure is
an event and a line of `kito status`, and the commits go with the next
push. A remote with commits of its own makes every push fail until a
person reconciles them: Kito does not pull, because what a pull brings
would reach the main lane without a lane.

### D-098 · A lane is changed by its owner, or by somebody who may integrate
Until now anybody who could use a repository could close anybody's lane.
The owner is the person the server authenticated when the lane was
opened; closing, scope, intent, renames, the request to integrate and the
decisions on its conflicts are theirs. Somebody who may integrate may do
all of these on any lane: that is the person a team turns to when an
owner is away. Writing files in a lane stays the owner's alone (D-089).

### D-099 · Small things found on the way
`kito` no longer reports a failure when whoever was reading its output
stops reading (`kito log | head`). `sync` is something a reader may ask:
it changes nothing of theirs, and `kito status` asks it. The rules for
agents have a third version: files written by 0.6 to 0.8 and never edited
are replaced, edited ones are left and the missing rule is named.

### D-100 · The why graph is read, not written
Everything it tells was already in the log: ops with their intent, lane,
author and time; lanes with their owner and task; checkpoints with their
checks; conflicts with their decisions; the texts before and after each
change. One fact was missing and is now kept: who asked for an
integration that went through (`approved_by` on the checkpoint, a field
added, no type changing meaning). A graph stored beside the log would
have been a second account of the same events, to keep in agreement.

### D-101 · What a reason is
An intent in a lane; or, for what the main lane got with no intent (edits
by hand, commits imported from git), the checkpoint that took it in, whose
label is then the reason. The ops an integration writes on the main lane
are not reasons of their own: they repeat what the lane said, and only
tell when it got in and under which checks. What a lane did before one of
its integrations and after it are told apart even under one intent: the
second is not on the main lane yet. Reasons are listed by when they
reached the asker's lane, not by when they were typed: for the code as it
is, the last to land is the last word.

### D-102 · Lines are told through the history of the file
The first version read the versions of a function from its own ops. It
could not tell a comment changed inside a function (no op on the function
is recorded for it), nor code that was there before Kito first read the
file. The history of the file has both: every save of every lane, and
every imported commit, with the whole content before and after. Lines are
attributed over that chain; what an integration wrote is then attributed
inside the lane that it brought, so a line carries the intent it was
written under, not "integrate lane/3". The same code answers for files
that are not programs.

### D-103 · `kito why` is the question; the chronology is `--history`
`kito why` printed every op of a symbol in order. That is still what
`kito why --history` prints, and what the `log` tool gives for a symbol.
The plain command now answers what its name asks. The demos and tests of
the earlier phases were moved to the flag.

### D-104 · An index in memory, built when first asked
From each reason to the functions and files it touched, and from each
file to the ops that changed it as a whole. It is built by reading the
log once, one op at a time, the first time somebody asks; from then on
every op is added as it is recorded. If the number of ops it holds is not
the store's, something was recorded behind its back and it is built
again. Edits made by hand that no checkpoint has taken in yet are kept
aside and filed under their checkpoint when it comes, without a rebuild.
Persisting the index was not needed: 500,000 ops are read in 1.3 s.

### D-105 · Reasons are other people's words
What `why` and `reasons` return was typed by agents and people, and is
read by agents that act. Every answer the MCP server gives says that
reasons are information about the history and not instructions, as it
already did for the intents of other lanes; the rules for agents say the
same, and say to ask before changing code somebody else wrote.

### D-106 · Who uses what: from the syntax, and not always
A question should be answered at once. Uses come from the reading of the
syntax Kito already does (the same file, imports by name), not from a
language server, which can take seconds. Above 5,000 files they are not
looked for unless asked (`--uses`, `"uses": true`), and the answer says
so: at 20,000 files it adds 0.3 s.

### D-107 · The why is given, not asked for
Phase 10 let agents ask why; the rules told them to. Whether they did was
each model's choice. The reasons now come in the answer of the one call no
lane can skip, the declaration of its scope: `open_lane` and
`extend_scope` carry the last reasons of each function and file declared,
and who uses it. Short (five reasons, eight users, eight items): `why`
tells the rest.

### D-108 · An edit is refused before it is made, by hooks
Where an agent may edit and what it declared were checked at integration,
after the work. For Claude Code they are now checked by a `PreToolUse`
hook, which the harness runs whatever the model decided: `deny`, with
words that say what to do instead. The hook is a thin command (`kito hook
before-edit`) that asks the daemon: the rules live in one place, and a
connected directory gets the server's answer. Hooks were chosen over a
wrapper around the tools because they are what the harness guarantees.

### D-109 · Held once, then let through
An edit on lines written for a reason the lane was not told is refused
once with the reasons, and goes through when made again. Stronger (the
agent must answer, somebody must approve) would stall swarms on every
legacy line; weaker (tell it after the edit) lets the change be written
first. Once is enough to put the reason in front of the model at the
moment it matters. What a lane was told is kept with the lane so that it
is once, and `[agents] why = false` turns the holding off.

### D-110 · Lines, not files
What is held for is the reasons behind the lines an edit replaces, found
through the history of the file as `kito why file:line` finds them, not
every reason the file ever changed for: an agent that fixes one function
of a long file is not told forty reasons. A file written whole is told
all of its reasons that were not told yet.

### D-111 · The integration is the net
A shell command, another editor, an agent with no hooks: none goes through
`before_edit`. So the MCP tool `request_integration` first asks for the
reasons behind what the lane replaced that it was never told, and answers
`read_first` instead of starting. It is in `kito-mcp`, not in the daemon's
queue: agents integrate through the tools, people through `kito integrate`,
and people are not held.

### D-112 · A hook that cannot ask lets the edit through
With no daemon to ask (stopped, a broken connection to the server), the
hook leaves with an error Claude Code does not treat as a refusal, and
says the edit was not checked. Blocking every edit when the infrastructure
is down would make people remove the hooks; the scope is still checked at
integration and a protected main lane still puts edits back.

### D-113 · Kito's own files are not an agent's to edit
An edit under `.kito/` other than a lane's working copy is refused by the
hook without asking anybody: the configuration there includes the rules
the agent is held to. To change a rule, it asks its person.

### D-114 · A shell command is told about, not undone
After `Bash`, a hook tells the model what the command changed in the main
directory and outside the scope of a lane. It is told for what was
recorded since the command started (what Claude Code says it took, and a
quarter of a second). Nothing is reverted: the main lane, when protected,
already puts files back, and a lane out of its scope is not integrated.

### D-115 · A server is known by its fingerprint
A team that sets up a server has no certificate authority and often no
public name. The server signs its own certificate and clients pin its
SHA-256 fingerprint, given with the token, as SSH does with host keys.
`--authority` is there for who has a real certificate. A server on the
same machine is trusted the first time: whoever could stand in for it
there already has the machine.

### D-116 · Encrypted unless asked otherwise, and then only locally
`serve` encrypts; `--plain` exists for the same machine and refuses any
other address, on both sides. A client and a server of different minds
say what the matter is instead of failing on bytes.

### D-117 · Landlock, through a helper that becomes the command
The sandbox had to work without privileges, without a program to install
and without `unsafe` outside `kito-store`. Landlock is in the kernel and
needs no privilege; `kitod sandbox` restricts itself and then execs the
command, so nothing runs between `fork` and `exec` in the daemon.
Namespaces would also give a private network, but need either privileges
or a root-mapped user namespace that changes who the tools think they
are.

### D-118 · What is hidden is said by naming everything else
Landlock allows; it does not forbid. To hide `.kito/`, the other
repositories and the keys, the policy allows every sibling of each hidden
path, up to the root. A file created later next to a hidden one is not
readable until the next command: the list is made for each run.

### D-119 · The checks get no network and no shared cache by default
A check that can call out can send away what it reads; one that can write
a cache other checks read can change what they run. Both are off unless
the repository says otherwise, and a check that fails confined says that
it ran confined, because the first thing to suspect is the confinement.

### D-120 · The daemon's socket asks for a key
A confined command could still connect to the daemon's socket and ask it
to lift the protection of the main lane: Landlock does not stop a
connection to a Unix socket. The socket now wants a key kept in `.kito/`,
which a confined command cannot read. Reading `.kito/` is what it takes
to command the daemon, as it was already what it took to change its data.

### D-121 · Where nothing can confine, the checks still run
On macOS and old kernels `auto` runs the checks as before and says so;
`required` is for a server that must not. Refusing by default would stop
every local repository that has checks on a system without Landlock.

### D-122 · Checked together only when the result is certain
Lanes are put in one run only if what each merge writes cannot depend on
the others: different files, and no rename or move anywhere. The copy
that is checked is then exactly what the main lane will hold when all of
them are in. A cleverer rule (different functions of one file) would need
the merge of the second lane on the result of the first before either is
integrated, which the merge cannot do today: it reads the main lane's
recorded history.

### D-123 · Integrated only if it is what was checked
After a run passes, each lane is prepared again on the main lane as it is
by then, and what its merge writes is compared with what was in the copy.
Anything else (the lane was saved again, the main lane moved, the merge
came out otherwise) sends the lane back to be checked by itself. Nothing
reaches the main lane on the strength of a check of something else.

### D-124 · A run that fails is halved
Halving finds one failing lane among n in about 2·log2(n) more runs, and
lets the halves that pass go in together. With one failing lane among
four it costs one run more than one at a time; with eight, one less.
`batch = 1` is there for repositories whose checks fail often.

### D-125 · One checkpoint per lane, as before
A run of five lanes makes five checkpoints and five git commits, each
with its lane's reason and author, and each report names the lanes it was
checked with. One checkpoint for the run would be simpler and would lose
who did what for which reason, which is what the why graph is made of.

### D-126 · A short wait for requests made together
When the repository has checks, the first request waits 15 ms before the
run is made up, so that `kito integrate a b c` and agents that finish in
the same instant are one run. Next to checks that take seconds it is
nothing; with no checks there is no wait and no batching.

### D-127 · Commits made with git come in through a lane
They could have been written straight onto the main lane, as an import
does. A lane gives them everything other work gets: the merge by
function with what the main lane did meanwhile, the checks, conflicts
that wait for a decision instead of failing, and a reason per commit in
the why graph. The lane starts from the checkpoint the commits were made
on, so the merge is a real three-way merge.

### D-128 · The result is a git merge
After the integration the remote must be able to fast-forward, or the
next push is refused. The checkpoint of the lane is exported with the
remote's last commit as second parent. Rewriting the commits Kito made
and had not pushed yet (a rebase) would also work, and would change
commits that are already checkpoints.

### D-129 · Joined even when nothing changes
When the commits change nothing here, there is nothing to integrate, and
yet the histories must be joined. A checkpoint with no operation of its
own does it. An empty checkpoint is new; the alternative was a remote
that refuses every push from then on.

### D-130 · Pull before push, and when asked
The daemon looks at the remote when it is about to push and when
`kito pull` is run. A timer would bring commits in sooner and would have
the daemon talk to a remote all day; it can be added without changing
anything else.

### D-131 · A change that does not apply stops the pull whole
When a commit's file is not, in the lane, as the commit found it, the
change is carried over by a three-way merge by lines. If that conflicts,
nothing is brought in and the error says which commit and file: half a
history replayed is worse than none.

### D-132 · A held lock is asked for again for two seconds
The lock of a store belongs to the open file, and a process forked by any
thread shares it until it executes its program, whatever close-on-exec
says. A store closed and opened again at once (`Repo::init` does it) could
find its own lock still held by a child of another thread that was about
to run git, and fail with `Locked`; the scope tests did whenever there
were fewer test threads than tests. The store now asks again every 10 ms
for up to 2 s before it says it is in use. A second daemon on the same
store is still refused, two seconds later. Locks that a fork does not
inherit (POSIX record locks) would not keep two opens in one process
apart, which the store relies on.

### D-133 · A process group is signalled directly, never through `kill`
A lane command that outlives its time is stopped with its whole process
group. It was done by running `kill -KILL -<group>`, and procps-ng 4.0.4,
the `kill` of Ubuntu 24.04, reads that as `kill(-1, SIGKILL)`: every
process of the user died, the shell and the terminal included. The group
is now signalled with the system call (`rustix`), as the checks of an
integration already were.

### D-134 · The state of a lane is found through first parents
A stacked lane starts from an op of its parent, and that parent may since
have been brought up to its own target, leaving the op behind its `since`.
Replaying "the parent's base plus its ops from `since`" would miss it.
Every op's first parent is the op before it on its lane, and a lane's
first op has `base_head` as first parent: replaying first parents back to
the main lane gives the state at any op, at any depth. Open lanes whose
head or base is that op answer at once.

### D-135 · A child keeps its base; it waits instead
When its parent lands, a child could be rebased onto the result. It is
not: the target then holds the child's base (the parent's work) and the
three-way merge from the old base brings only what the child did.
Rebasing would rewrite ops for nothing. What a child needs is a rule:
it lands only on a lane that holds the op it started from, found through
both parents of the target's ops; ops are newer than those they are made
on, so the walk stops at ops older than it.

### D-136 · Landing on a lane: no checkpoint, no checks, at once
Checkpoints are of the main lane until permanent lanes (phase 16). The
checks run on what reaches the main lane; on a lane that is not shared
they would check a state nobody deploys. With nothing to wait for, the
daemon lands it when asked rather than behind the lanes waiting for the
main lane's checks: that is the queue per target of ADR 0001, for now.
Who may land on a lane is who may change it (its owner, or an
integrator).

### D-137 · The lane landed on takes on the scope
It brings that work further, and would otherwise be refused at its own
integration for changing what it never declared. The entries are added
as the child declared them.

### D-138 · A parent closes when its children's base has landed
Closing a lane whose work a child was built on would leave the child on
nothing. The close is refused while it is so; once the work has landed,
the children are moved where it went. Adopting the parent's ops into the
child was the other way, and would make the child bring work it did not
do.

### D-139 · Lines that came through a lane are followed into it
On the main lane an integration's lines are attributed to the lane
integrated (phase 10). A lane that landed on that lane did so with an op
of its own there, whose lines are followed in the same way into the lane
that landed, down to eight levels. Without it, a page written by Bob in a
lane on Alice's would read as Alice's.

### D-140 · `checkpoints()` keeps meaning the main lane's
Every reader of the checkpoints until now (export, log, status, rollback,
pull, the why index) means the main lane's. Rather than filter in each,
the store's `checkpoints()` returns those, and `lane_checkpoints` and
`all_checkpoints` are new: a reader that forgets permanent lanes stays
right about main.

### D-141 · A permanent lane is a lane with a name
Not a second kind of lane: it has a working copy, ops, a base, and lands
like any other. What differs: it has no scope (it holds what landed on
it), it is not closed, its landings are checkpoints of it, and it is
named. Main stays the lane without a record.

### D-142 · Catching up keeps the base
Moving staging's base to main's head would need staging's own work
re-expressed on top. The merge is written on staging instead, and its
base stays: when staging lands, main's changes are on both sides and the
merge by function takes them as one. The next landing on main brings
staging up to main anyway.

### D-143 · A permanent lane's commits, made whole
The branch of a permanent lane is written from the tree at each of its
checkpoints, every file, rather than replayed op by op as main's is:
permanent lanes have few checkpoints, and their ops start from different
states of main. Its first parent is the lane's commit before; its second,
main's commit, when the lane was brought up to a new state of main, or
the commit a catch-up merged.

### D-155 · An edit by hand is dated by its files too
After a shell command an agent is told what the command changed in the
main directory. "Since the command started" was judged by when an edit
was recorded, and a save can be recorded well after it was made (a busy
machine, a directory that is scanned): a test failed that way under
load, and so would an agent have been blamed. An edit by hand now keeps
when its files say they were written, and counts for a command only if
it was recorded since the command started and its files are not clearly
older. Files are dated by a coarser clock than the one a command's start
is read from, so "clearly" is 50 ms. Files put back (a protected main
lane) or removed say nothing, and are judged by the record alone.

### D-154 · A server's plugins are told by whoever did the thing
What an administrator does to a server is done by a `kito-server`
command, not by the server that is serving: the command that added a
person or a repository tells the plugins itself, after it is done and
before it leaves, one plugin at a time. So an event is told once, by the
process that knows it happened, with no queue to keep between processes;
and a plugin that fails is said on the administrator's terminal, where
somebody reads it, and written under the server's directory
(`plugins/<name>/last-failure`) until it runs well again. What was asked
is not undone by a plugin that fails. Events of the server that serves
(somebody signing in) will be told by it, in order, as a repository's
are (D-152). A command a plugin adds is given its own name first, so
that a plugin with several knows which was asked. Permissions are one
list for both: a repository gives only what is a repository's.

### D-153 · A plugin's tools are listed when asked, under its name
`kito-mcp` asks the daemon which plugins are enabled each time an agent
lists the tools or calls one that is not Kito's: a plugin enabled or
disabled while an agent works is there, or gone, at its next question,
with nothing to restart. The tools of a plugin come after Kito's own, as
`kito.<plugin>.<tool>` (with `--tool-prefix kito_`, `kito_<plugin>_<tool>`),
and their descriptions say that they are the plugin's: what a plugin
tells an agent is the plugin's word, not Kito's. `kito-mcp` runs the tool
itself, with the plugin's key, so the daemon answers the tool what the
plugin was given. An assistant is set up by `kito assistant add`, which
runs the plugin's own `assistant setup` in the repository and shows what
it says: Kito knows nothing of that assistant's files. Installing a
plugin by name from a release is left for when a plugin has one: code
that downloads and unpacks is not written against nothing.

### D-152 · Gates are asked with the checks; events are told after the fact
A gate of a plugin runs where the checks do and as they do: in the copy
of the merged result, after the commands, confined, stopped when its time
is up, and in the same run when several lanes are checked together (it is
then told every lane in the copy, and a run that fails is split as for
any check). So a gate needs no machinery of its own, and what it said is
in the verification of the checkpoint. It is told what it needs on its
input and given no key. "Wait" is a report that did not pass and says
why (`waits`): the lane is left `awaiting_approval` with that as what is
missing, before the review is asked. Events are told by a thread that
reads the daemon's own events and hands each to a queue per plugin: one
at a time and in order for a plugin, never holding up what happened, and
never anybody else's plugin. What a plugin last failed at is kept in the
daemon until it runs well again, and is forgotten when the daemon stops.
A plugin is not told its own `plugin_failed`. The lane of an event is
named as the answer names it (`lane/1`), not as it was typed.

### D-151 · A plugin says a key of its own, and a method needs one permission
The first line on the daemon's socket is a key. The socket's own key is
whoever can read the repository's data, as before; the key of a plugin
(`.kito/plugins/<name>/key`, made when it is enabled, readable by its
owner) is that plugin, and every request on the connection is checked
against what the repository gave it before the daemon looks at it. Each
method needs exactly one permission; one the list does not know needs
`admin`, so that a method added later is closed to plugins until somebody
says otherwise. `plugin_key` and `shutdown` are for no plugin, whatever it
was given. Kito's own client says the key in `KITO_PLUGIN_KEY` when there
is one, so that `kito` run by a plugin is that plugin too. A test reads
`docs/api.md` and the daemon's source: every method is written down under
the permission it needs, and nothing is written down that is not
answered. A plugin is trusted as a program its person installed (it could
read the socket's key from the disk): the permissions are about what a
plugin is meant to do, and what a mistake of it can reach. The daemon of
a connected directory does not answer plugins yet.

### D-150 · The owner's approval is given and does not count
An approval by the owner of a lane, or by an agent that works for them,
is recorded and shown as not counting ("the lane is theirs"): refusing it
would stop a person from saying anything on the lane of their own agent,
which is most of what review is on one machine. Their request for
changes stands like anybody's and stops the lane, also where no approval
is required: it is how a person holds back their agent's work. An
agent's verdict is refused outright unless `[review] agents = true`. A
verdict is a record of its own (`.kito/verdicts`, numbered); the last one
of a person on a lane stands, and `withdraw` is a verdict that says
nothing stands. Approving writes nothing to the target: `integrate` asks
the review again when it runs, so an approval that went stale meanwhile
is seen then. The people whose approval counted are on the checkpoint
(`reviewed_by`, optional and last), in the why of that code, and in the
exported commit as `Reviewed-by:` trailers.

### D-149 · A comment on lines is found by what the lines said
A comment on lines keeps their text. Shown, it is looked for in the
lane's file as it is: found, the comment is on those lines where they
are now (the nearest place, when the same lines are there twice); not
found, it is `outdated` and shown with what it quoted. No positions are
kept up to date through the edits, and nothing is guessed: either the
lane still has the lines the reviewer saw or it does not. A comment on a
function, with no lines, is outdated once the lane recorded an op on that
function after the comment (`at_ops`, the count of the lane's ops then,
as a proposal has). The first edit after a comment is held back once
with every comment the lane's agent was not told, whatever file the edit
is in: a review is about the lane, and "add a test" is about no line.
Comments have a log of their own, apart from the verdicts' (a keyed log
has one kind of key).

### D-148 · The diff of a lane is against what it started from
`lane_diff` compares the lane as it is with the state it started from
(or was last brought up to), not with its target as it is now: what the
target got meanwhile is not the lane's doing, and would show as the lane
undoing it. That is said apart, as `behind`: the files the target changed
since, which is what there is to count without merging. The symbols of
the diff are those the lane's ops touched plus those the two symbol
tables have differently; a function that only follows its file to a new
name is the file's rename and is not listed. The working copy is scanned
before the diff is made: a review is of what is there. Anybody who may
read may ask (ADR 0004).

### D-147 · A file's history is followed through its names
The why of a file was gathered by its path: after a rename its lines
came out with no reason, and a new file given the name an earlier one
had took that one's history. A file's changes are now gathered under its
name since it last got it, and under each name it had before, back from
the rename that gave it the next one. Only renames Kito saw as renames
are followed (the same content under a new name, `FileRename`): a rename
made together with an edit is a removal and a creation, and the story
starts there.

### D-146 · A note is told once, with the reasons
An agent about to change code with a note on it is told the note, in the
same answer as the reasons, and the edit is held back once, as for the
reasons (phase 11). Notes are remembered as told with the reasons; the
answers to `open_lane` and `extend_scope` count as telling them. A note
on lines is the note of the function they are in: lines move with every
edit, functions keep their identity. The notes log is not part of the
index's watermark: a note is durable once written, and nothing in the
index points at it, so an older binary that ignores the file loses
nothing else.

### D-145 · An intent that says nothing is refused
The intent is what the next agent is told before it changes the code:
"wip" or "fix" tells it nothing. An intent made only of words that say
that something changed but not why (`wip`, `fix`, `update`, `changes`,
`minor`, `stuff`…, with fewer than three letters in all) is refused when a
lane is opened or an intent is set, with an example of one that says
something. A list of such words, rather than a minimum length: "round half
up" is short and says it all. Intents Kito writes itself are taken as they
are: a git commit's subject is its author's (phase 14), and the text of an
integration or a permanent lane is Kito's.

### D-144 · Staging landing on main is a merge with staging's branch
When the last checkpoint of the permanent lane is the lane as it lands,
main's checkpoint records its commit (`git_merge`, as for a pull) and is
exported as a merge: `git log` shows what came through staging, and a
branch that follows staging fast-forwards to main.
