# 0004 · Review of a lane

Status: accepted
Date: 2026-11-21

## Context

A lane goes in when its merge and its checks pass and, on a server,
somebody who may integrate says so (D-095). That is all the say a person
has: yes, by integrating, or nothing.

- Nobody can see what a lane changes without going to its working copy.
  There is no diff of a lane: `kito lane status` lists the files.
- Nothing can be said about a lane where the lane is. What a person
  thinks of an agent's work goes through a chat, and the agent working in
  the lane is not told.
- "Approval is one person integrating" (phase 9's limits): no number of
  approvals, no approval by somebody who may not integrate, no "not like
  this".

Two new kinds of record are added to the store (comments and verdicts),
and one configuration choice changes what `awaiting_approval` means: the
ADR rules ask to write both down first.

Kito has no interface of its own for this, and gets none here: review is
data in the store and questions the daemon and the server answer. A web
view is another program, optional, that asks the same questions as the
command line (decided with the person on 2026-11-20).

## Decision

### 1. The diff of a lane

`lane_diff(lane, paths?)` answers what the lane changes against what it
started from; what its target got since is said apart, as `behind`:

```rust
pub struct LaneDiff {
    pub lane: String,
    pub onto: String,            // "main", "lane/3", "staging"
    pub base: String,            // ckpt#12, or the op of a parent lane
    pub behind: usize,           // files its target changed since
    pub files: Vec<FileDiff>,
}

pub struct FileDiff {
    pub path: String,
    pub was: Option<String>,     // its name before, if the lane renamed it
    pub change: Change,          // Added | Changed | Removed | Renamed
    pub binary: bool,
    pub before: Option<String>,  // blob IDs: `content` gives the texts
    pub after: Option<String>,
    pub added: usize,            // lines
    pub removed: usize,
    pub hunks: Vec<Hunk>,        // lines, with 3 of context
    pub symbols: Vec<SymbolChange>,
}

pub struct SymbolChange {
    pub symbol: String,          // src/shop.ts:total
    pub change: Change,          // a rename or a move is one change, not two
    pub was: Option<String>,
    pub lines: Option<(u32, u32)>, // in the lane's text; none: removed
    pub intents: Vec<String>,    // why, as the lane's ops say
}
```

A function that moved to another file, or was renamed, is one change with
its two names: Kito knows it is the same function. `kito lane diff <lane>
[path…] [--stat] [--json]`, and the MCP tool `lane_diff`.

### 2. Comments

```rust
pub struct Comment {
    pub id: CommentId,              // comment#7, numbered like notes
    pub lane: LaneId,
    pub on: Option<CommentTarget>,  // none: about the lane as a whole
    pub text: String,
    pub by: Author,
    pub person: Option<Author>,     // whom an agent works for
    pub at_ms: u64,
    pub at_ops: u64,                // how many ops the lane had when it was written
    pub reply_to: Option<CommentId>,
    pub resolved: Option<(Author, u64)>,
}

pub struct CommentTarget {
    pub path: PathBuf,
    pub symbol: Option<SymbolId>,   // the function the lines are in
    pub lines: Option<(u32, u32)>,  // in the lane's text then
    pub quoted: String,             // those lines, as they were
}
```

- **It stays with the code it is about.** A comment on lines keeps what
  the lines said (`quoted`). While the lane's file still has those lines,
  wherever they moved, the comment is on them, at the place nearest to
  where they were; once it does not, the comment is `outdated` and shown
  with what it quoted: it is neither moved by guessing nor dropped. A
  comment on a function follows it through renames and moves, and is
  `outdated` once the lane recorded something on that function after
  `at_ops`; one on a file, once the file is gone (D-149).
- **The agent in the lane is told.** Comments that are not resolved are
  part of `kito lane status` (and of the MCP tool `status`), and of what
  the hooks tell an agent before it edits (phase 11): the first edit
  after a comment is held back once, with the comments not told yet. What
  the lane's own agent wrote is not told back. A comment is how a person
  tells an agent in a lane what to change.
- **Answers** join the exchange a comment started: an answer to an answer
  is an answer to the first comment.
- **Resolved**, not removed: by who wrote it, by the lane's owner, or by
  somebody who may integrate. An answer does not resolve.
- `kito lane comment <lane> "<text>"` (`--on <target>`, `--reply
  comment#7`), `kito lane comments <lane>` (`--all`), `kito lane
  resolve-comment comment#7`; the MCP tools `comment`, `comments` and
  `resolve_comment`; the daemon's methods of the same names. Events:
  `commented`, `comment_resolved`.

### 3. Verdicts

```rust
pub struct Verdict {
    pub id: VerdictId,          // each is a record; a person's last one stands
    pub lane: LaneId,
    pub by: Author,
    pub person: Option<Author>,
    pub kind: VerdictKind,      // Approved | ChangesRequested | Withdrawn
    pub text: Option<String>,
    pub at_ms: u64,
    pub at_ops: u64,            // what was looked at: the lane's ops then
}
```

- One verdict per person and lane: the last one stands.
- **A verdict is about the lane as it was.** When the lane changes after
  `at_op`, an approval is `stale`: shown, not counted. (`[review] keep =
  true` counts it still, for teams that want that.) A request for changes
  stands until whoever made it gives another verdict or withdraws it.
- **The owner's own approval does not count**, nor one by an agent that
  works for the owner: a review is somebody else's. It may be given, and
  is shown as not counting. The owner's request for changes stands like
  anybody's: it is how a person holds back the lane of their own agent.
- **A verdict by an agent** is refused unless `[review] agents = true`
  (not by default: a verdict is a person's word); where it is taken it is
  its person's, once.
- `kito lane approve <lane> ["<text>"]`, `kito lane request-changes <lane>
  "<text>"`, `kito lane withdraw <lane>`, and `kito lane review <lane>`: where
  it stands. The MCP tool `review` reads that, and gives a verdict where
  agents may. The daemon's `verdict` and `review`. An event, `reviewed`.
  `kito lanes` and the board say where each lane's review stands.

### 4. What integration asks

```toml
[review]
required = 0      # approvals a lane needs before it is integrated
agents = false    # whether an agent's verdict counts, as its person's
keep = false      # whether approvals survive a change to the lane
```

On a server the repository's are set there (`kito-server repo set shop
--approvals 1`), like its checks (D-096): no client changes them.
`[lanes.<name>]` may carry its own `required` for what lands on a
permanent lane.

Asked to integrate, a lane is merged and checked as today, then:

- a request for changes that stands, or fewer approvals that count than
  `required`: `awaiting_approval`, saying what is missing ("1 of 2
  approvals; changes requested by bob"). Nothing is written.
- otherwise as today: integrated if who asks may integrate, and
  `awaiting_approval` if not (D-095).

**Approving and integrating are two actions.** An approval writes
nothing to the target, whoever gives it: a lane that has its approvals
still waits until somebody who may integrate runs `kito integrate
<lane>` (the person who approved, or another), and the merge and the
checks run again on the target as it is then. The board says which
waiting lanes lack nothing but that ("approved, to integrate"). With
`required = 0` and no server, nothing changes for anybody.

### 5. Where it is kept, and who may

- Logs of their own, kept like the notes log: `.kito/comments` (the last
  record of a comment wins; `LogKind::Comments`) and `.kito/verdicts`
  (the last of a person on a lane wins). The store's format does not
  change, and an older Kito ignores the files.
- On a server: reading needs `read`; commenting and giving a verdict need
  `write`; `required` is counted among people with `write` or more.
- A closed lane keeps its review. `why` on code that came in through a
  reviewed lane says who approved it (`reviewed_by`), next to the intent.
- Not exported to git: the commit of an integration gets a trailer,
  `Reviewed-by: Name <email>`, per approval that counted, and that is all.

## Consequences

- A repository without `[review]`, and every existing lane, behaves as
  before; an older binary opens a repository that has reviews and shows
  none.
- `awaiting_approval` now has two reasons. Its answer says which
  (`missing` in the result); agents' rules written by 0.9 to 0.18 still
  hold ("leave the lane open, say that it waits").
- An approval never changes a lane's target: integration stays one
  action, with its roles (D-095).
- A comment on code the lane later removed stays, `outdated`, until it is
  resolved.
- Not done here: a web view (another program, in its own repository,
  using `lane_diff`, `comments`, `review` and the events); comments on
  code outside a lane (that is a note); owners of paths whose approval is
  required for those paths; review of a lane before it has asked to be
  integrated is possible, and nothing asks for it.

## Order of work

One phase, three parts, each with its tests and one commit, as phase 18:

1. the diff of a lane;
2. comments, and telling the agent;
3. verdicts, `[review]`, the server's side, the demo and the documents.

Released as 0.19.0: 0.18.0 is taken by the phase that went first.
