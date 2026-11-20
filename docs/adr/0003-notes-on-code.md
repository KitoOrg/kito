# 0003 · Notes on code

Status: accepted
Date: 2026-11-20

## Context

The why graph tells why code is the way it is from the intents of the
changes that made it. Some reasons belong to no change:

- "Called by the nightly billing job, outside this repository: do not
  rename."
- "Slow on purpose: the payment provider limits us to 2 requests a second."
- "Kept for the mobile app 3.x until March."

Today they can only be written as comments in the code, which agents may
not read in time, or remove with the code, or as an intent of some
unrelated change. An agent about to change the code is told the intents,
not these.

A new kind of record is added to the store, which the ADR rules ask to
write down first.

## Decision

**A note** is a short text that a person or an agent attaches to code,
without changing it:

```rust
pub struct Note {
    pub id: NoteId,                 // note#7, numbered like conflicts
    pub on: NoteTarget,
    pub text: String,
    pub by: Author,                 // who wrote it (person, or agent)
    pub person: Option<Author>,     // whom the agent works for, when known
    pub at_ms: u64,
    pub lane: Option<LaneId>,       // where it was written, if in a lane
    pub removed: Option<(Author, u64)>,
}

pub enum NoteTarget {
    /// A function or a class: the note follows it through renames and
    /// moves, as its history does.
    Symbol { id: SymbolId, file: PathBuf, name: String },
    /// A file, or a directory (`src/billing/`).
    Path(PathBuf),
}
```

- **On lines**, a note is the note of the innermost function they are in,
  or else of the file: lines move with every edit, functions keep their
  identity. `why` on those lines shows it.
- **Stored** in a log of its own, `.kito/notes`, kept like the conflicts
  log (the last record of a note wins). An older Kito ignores the file: it
  shows no notes, and reads everything else as before. The store's format
  does not change.
- **Removed**, a note is kept, marked with who removed it and when; it is
  no longer shown.
- **Shown** wherever the why graph is: `kito why`, the MCP `why` tool, the
  `why` that `open_lane` and `extend_scope` answer with, and the reasons
  an agent is told before it edits code (phase 11). In the why answer they
  are `annotations` (`notes` there already holds Kito's own remarks).
  Notes on a directory are shown for everything under it.
- **Written** with `kito note <target> "<text>"`, `kito note --list
  [<target>]`, `kito note --remove note#7`; the MCP tool `note`; the
  daemon's `add_note`, `notes`, `remove_note`. On a server, writing and
  removing a note needs the `write` role; reading, `read`. An event,
  `noted`, for whoever watches.
- **Not exported to git**: they are Kito's, like the intents.

## Consequences

- Nothing changes for repositories without notes; an older binary still
  opens a repository that has some.
- A note on a function that is removed stays, and says so ("on a function
  that no longer exists") until somebody removes it.
- Not done here: notes in git (`git notes`), notes on a single line that
  follow it through edits, replies to a note (that is review, phase 17).
