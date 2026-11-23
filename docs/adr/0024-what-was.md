# 0024 · What was: closed lanes, checkpoints and files as they were

Status: accepted
Date: 2026-11-23

## Context

A lane that closed kept its record and its operations, and nothing could
be asked of it but its comments and its review: `lane_status`,
`lane_diff`, `lane_files`, `log`, `why` and `why_graph` refused it, and
`lanes {all}` said it had recorded no op and changed no file. Nothing
said when it was integrated, as which checkpoint, by whom, or that it
was given up. There was no list of the checkpoints as a history of
commits is listed, no way to see what one checkpoint changed, and no way
to see a directory as it was at one of them. A task kept when it was
asked for and when it last changed, and nothing of what happened in
between. The owner asked for a closed lane to show everything,
read-only; for the checkpoints to be the history, a page per checkpoint
with its files and the lane it came from; for the files to be browsed
as they were at any checkpoint; and for a task to say what happened to
it, in order. This is the daemon's side of that: what the web client
and the CLI ask.

## Decision

**What is written, and where.** Everything new is an optional field: a
record written before is read as it was (CBOR is keyed by name; the
fields default).

- A lane's record (`Lane`, in the store's lane records) gains
  `integrations`, each `{into, checkpoint, at_ms, by}`, appended when its
  work lands (on the main lane, a permanent lane, or a lane), with the
  checkpoint made, if one was, and who let it in (only when a person was
  named: the one a server said may integrate and asked); `closed`,
  `{at_ms, by}`, set when it is closed (`by`: the person a server named);
  and `ends_kept`, set for every lane opened from 0.41 on. An agent that
  integrates or closes its lane on somebody's machine was let in or
  closed by nobody Kito knows of: `by` is left out.
- A checkpoint gains `at_ms` (when it was made; for those imported from
  git, the commit's time) and `integrated` (the lane whose integration
  made it).
- A task (`.kito/tasks.json`) gains `history`, `[{at_ms, by, what, …}]`:
  `asked`, `edited` (`field`: `title` or `text`), `for` (`for`: whom, or
  `null`), `taken_up` and `thinks_done` (`agent`), `comment` (`n`),
  `lane_opened`, `lane_integrated` (`lane`, `checkpoint`), `lane_closed`,
  `done`, `reopened`, `dropped`. The daemon writes them where the task
  methods change a task, and where a lane of one of the repository's
  tasks is opened, integrated or closed (`by` there: the person a server
  named, else this machine's identity, as the task methods have always
  said who). Every load, change and save of `tasks.json` is made under
  one lock of the daemon, each write through a file of its own renamed
  over it; a file that cannot be read is an error for every task
  request, and is never written over.

**What is worked out, never guessed.** For a lane from before
(`ends_kept` not set), its integrations are read from the history once
per daemon: an op a lane's agent recorded on another lane under the
reason `integrate lane/N: …` (or `integrate lane/N (agent)`), whose
second parent is the lane's work, starts one; the ops after it under the
same reason are its rest; a checkpoint whose head is the last of them is
its checkpoint, and that checkpoint's `approved_by`, when it has one, is
`by`. An integration that recorded no op is not found, and is not said.
Before a lane from before records an integration or is closed, what the
history says is copied into its record and `ends_kept` is set. A lane
closed before 0.41 says `closed: {}`: when and by whom were not kept. A
checkpoint from before says the lane it came from when its head's reason
names one whose agent made the checkpoint; its `at_ms` is the time of the
last op it holds (said as such in the API). A task from before tells
`asked` (its `at_ms` and `by`) and its comments, which have their times,
and from the first thing that happens to it keeps everything after.

**A closed lane answers what reads it**, from the store:
`lane_status` and `lanes {all}` (its `ops`: every op it recorded; its
`files`: every file those touched), `lane_diff` (its last op against
what it was last brought up to: what it had not integrated), `lane_files`
(its files when it closed), `log {lane}`, `why {lane}` and `why_graph
{lane}` (the lane stands, for that question only, as it was when it
closed: nothing is scanned, nobody is told). What writes still refuses
it with -32022, which keeps its meaning for a lane that never was, and so
do `changes_since_review` and `reasons_behind`.

**New `read` methods.** `checkpoints {[lane], [limit], [before], [path]}`
lists those of the main lane or of a permanent lane, newest first;
`checkpoint_info {checkpoint}` is one, with its files; `diff {to,
[from], [paths]}` compares two points (a checkpoint, `main`, a lane, a
closed one included) with the differ of `lane_diff`; `tree {[at],
[path]}` is a directory at a point, each entry with the last checkpoint
that changed it, and its README; `lane_files` takes `at`. What a
checkpoint changed is what is recorded on its lane after the checkpoint
before it, up to its head; for a permanent lane brought up to the main
lane meanwhile, what that brought too.

**How fast, and how much is kept.** The first question about a lane's
checkpoints replays its files' history once and keeps, in the daemon's
memory, what each checkpoint changed (the files, with their contents'
IDs before and after) and, on the main lane, the place of each
checkpoint's head; later questions read only what was recorded since.
Whole states (the files and the symbols) are kept for the last **8**
asked for, and no others, whatever the number of checkpoints: a
checkpoint's state is rebuilt when it is asked for, from the nearest of
those 8 before it on the main lane, else from the first op. The lines of
each file are counted when first asked. `tree` looks back through at
most 1,000 checkpoints for the last change of each entry, in one pass
that stops when every entry has one. `diff` looks for its reasons only
when `from` is behind `to`: two checkpoints of the main lane are read
as the range between them; otherwise `from` is looked for along first
parents and in the lanes an integration brought, at most 5,000 ops; a
`from` recorded later than `to` is not looked for. Nothing new is
written on disk. Measured: see the end.

**Tasks named as people write them.** When a lane is opened, and when an
intent is given a task, `task #55`, `Task #55` and `TASK#55` name this
repository's task 55 **when it has one**: the lane then keeps `task#55`,
and the task must be open. When it has no task 55 the text is kept as it
was said, as before: somebody's label for a tracker of their own, or what
an older assistant was told to write. `task#55` written so, with no such
task, is refused as before. A refusal says how to go on (open the lane
without a task, `kito task reopen task#55`, `kito task list`). What links
a lane to a task everywhere (a task's lanes and state, its comments told
to the lane, its history) reads what the lane keeps as 0.40 did
(`task#55`, `#55`, `55`): a lane opened before as `task #55` is not task
55's now. The MCP tools and the CLI's help say `task#55`.

## What it does not do

- It does not list every op of a checkpoint under it: a checkpoint is
  its files and their lines, and `log` has the ops.
- It does not give, in one request, the last change of every file of
  the repository.
- It keeps no index on disk: what is worked out is worked out again by
  a daemon that starts.
- It does not keep how each check command ended: a checkpoint keeps its
  commands, whether they all passed, and how long they took.
- `diff` gives the reasons of the symbols (`intents`) only when one
  point is behind the other, as said above; otherwise it compares their
  files and symbols without them, and without renames of files. Kito's
  own `integrate lane/N: …` is left out where the lane's reasons are
  there.
- It changes nothing of how a lane is integrated or closed, beyond
  keeping what is said above.

## Going back to 0.40

Nothing breaks: 0.40 reads every record 0.41 writes, the new fields
unknown to it. What 0.40 writes again loses them: a task changed by 0.40
loses its `history`; a lane's record written again (any change to an
open lane) loses `integrations`, `closed` and `ends_kept`; a checkpoint
written again (when it is exported to git) loses `at_ms` and
`integrated`. 0.41 then reads them as it reads what came before it: the
integrations worked out from the history, no closing time, a task's
history from when it was asked for and its comments, a checkpoint's time
from its last op.

## Measured

On 2,801 ops and 101 checkpoints, all on the main lane, by a throwaway
test: `checkpoints` (50) 435 ms the first time, 4 ms after; `diff`,
`tree` and `lane_files {at}` of a past checkpoint 1 to 35 ms; `diff`
from the second checkpoint to `main` 270 ms (round 1, kept for
comparison; see the report of the round that changed it for the times
now).
