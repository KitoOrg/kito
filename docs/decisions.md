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
