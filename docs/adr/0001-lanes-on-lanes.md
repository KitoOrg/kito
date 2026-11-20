# 0001 · Lanes on lanes: stacked lanes, targets, permanent lanes

Status: accepted
Date: 2026-11-20

## Context

Every lane starts from the main lane and lands on it. The code and the
data say so in many places:

- `Lane.base` and `Lane.base_head` are documented as "of the main lane",
  and `state_at` (kitod/src/lanes.rs) replays only ops of `LaneId::MAIN`:
  a lane cannot start from work that has not landed.
- `Checkpoint` has no lane: every checkpoint is a checkpoint of the main
  lane, and the export (kito-git) writes all of them, in order, onto one
  branch.
- Integration merges `base_tree` against `main.tree`, writes with
  `with(LaneId::MAIN)`, and goes through one queue (D-046) under one set
  of checks (`[verify]`).
- Conflicts have the sides `[main, lane]` and `Resolution::Main`.
- Protection (`[main] protected`), pull and push (`[git] branch`), the
  why graph ("landed" means "on main"), the server's `Integrate` role and
  the tools of the CLI and MCP all name the main lane.

Three things asked for do not fit:

1. **Stacked lanes.** An agent wants to build on another agent's lane
   before it lands (the API before the UI that uses it). Today it has to
   wait, or copy the work.
2. **A lane that lands on another lane.** Several agents finish pieces of
   one feature in lanes of their own, which come together in the
   feature's lane before the feature lands on main.
3. **Permanent lanes.** A `staging` next to `main`: lanes land on it, CI
   checks and deploys from it, and it is brought onto main when it is
   good. It never closes, has checkpoints of its own and a git branch of
   its own.

## Decision

**One model for the three: every lane has a parent and a target.**

- The *parent* is the lane it starts from. The *target* is the lane it
  lands on. Both are the main lane unless said otherwise, which is what
  every lane is today.
- A stacked lane is a lane whose parent is another open lane. Its target
  is its parent unless said otherwise.
- A lane may target any lane that is open or permanent, its parent or
  an ancestor of its parent. A lane is never its own ancestor.
- A *permanent lane* has a name, cannot be closed, has checkpoints and
  may be the parent and target of other lanes. `main` is the permanent
  lane that always exists (`LaneId::MAIN`, unchanged).

**The data (kito-core).** Optional fields at the end, absent meaning
what they mean today:

```rust
pub struct Lane {
    // … as today; `base` and `base_head` now read "of the parent lane".
    /// The lane this one started from. None: the main lane.
    pub parent: Option<LaneId>,
    /// The lane this one lands on. None: its parent.
    pub target: Option<LaneId>,
    /// Set for a permanent lane: its name ("staging"), unique, never
    /// "main", and the name of its git branch unless configured.
    pub name: Option<String>,
}

pub struct Checkpoint {
    // … as today.
    /// The lane the checkpoint is a checkpoint of. None: the main lane.
    pub lane: Option<LaneId>,
}
```

A permanent lane has a lane record, as other lanes do; the main lane
keeps having none (`.kito/lanes/main/state`, D-024).

`base` stays a `CheckpointId`: the last checkpoint of the main lane below
the lane, whatever its parent. That keeps every existing reader of `base`
correct. What a stacked lane starts from is `base_head`, which may now be
an op of the parent lane.

**State.** `state_at(lane, op)` replaces the main-only `state_at`: the
state of a lane at one of its ops is the state of its parent at the
lane's `base_head`, plus the lane's ops from `since` up to that op, down
the chain to the main lane. Lane working copies are made from that tree;
files equal to the main directory are still cloned from it.

**Integration.** The merge is the same three-way merge, with the
target's tree where the main lane's tree is today: base = the lane's
`base_tree`, "main" = the target as it is now, "lane" = the lane.
kito-merge's `Input.main` and the conflict's first side are the target;
`Resolution::Main` keeps its encoded name and means "the target". The result is written on the target with its own author, and
is a checkpoint of the target when the target is permanent. Landing on a
lane that is not permanent writes ops on that lane, as a capture would,
under the intent of the lane that landed.

- **One queue per target.** Lanes landing on `staging` do not wait for
  lanes landing on `main`; batches (phase 13) are made per target.
- **Checks per target.** `[verify]` stays the default; a permanent lane
  may have its own (`[lanes.staging.verify]`). Landing on a lane that is
  not permanent runs no checks: the work is not on a shared lane yet,
  and it is checked when it reaches one.
- **A lane lands only on a lane that holds what it was built on**: the
  op its `base_head` names must be an ancestor of the target's head
  (through both parents of the ops that took lanes in). A lane stacked on
  `lane/2` and targeting `main` waits until `lane/2` has landed on main;
  it is refused with the name of the lane it waits for. Otherwise it
  would bring its parent's work along unasked.
- **A child keeps its base when its parent lands.** Its base is the state
  of the parent at the op it started from; the target, once the parent
  has landed, holds that work too, so the three-way merge from the old
  base takes in only what the child did. Nothing is rewritten.
- **A parent closes only when what its children were built on has
  landed** on its target. Its children are then moved there: their
  parent becomes the parent's target, and if they targeted the parent
  they now target its target. A parent whose work has not landed cannot
  be closed while it has open children: no work is lost by closing a
  lane under another.
- The state of a lane at any op is the replay of the first parents of
  that op back to the start: a lane's first op has its `base_head` as
  first parent, an integration has the target's head. That holds at any
  depth, and after the parent was brought up to its own target.

**Permanent lanes.**

- `kito lane create staging [--from main]`: a permanent lane, starting
  from a checkpoint of its parent (main by default).
- `kito lane open --on staging` opens a lane on it; `--target` names a
  target other than the parent.
- `kito integrate staging` lands the permanent lane on its target (main)
  without closing it, as one checkpoint of the target; staging then
  starts again from that checkpoint, as a landed parent's children do.
- `kito catch-up staging` brings main's later checkpoints into staging
  (the same merge, the other way): what landed on main directly does
  not have to go through staging.
- Protection is per permanent lane: `[lanes.staging] protected` next to
  `[main] protected`, which stays.

**Git.** Each permanent lane is exported to its own branch
(`refs/heads/<name>`, or `[lanes.<name>] branch`), its checkpoints in
order; the checkpoint that lands staging on main is a git merge of the
two branches, as phase 14's merges are. Lanes that are not permanent are
not exported, as today. Push and pull (phase 14) work per permanent lane
that has a branch on the remote.

**Why graph.** "Landed" becomes "landed on <lane>": a reason follows a
change to the lane it landed on and from there, when that lane lands, to
its target. The blame chain of a lane goes through its parents.

**Server.** The `Integrate` role keeps meaning "may land work", on any
lane. A permanent lane may restrict who lands on it
(`[lanes.main] integrate = ["ci"]`); the default is the role, as today.

## Consequences

- **Existing data** reads as it is: no field set means parent and target
  `main`, checkpoints of `main`. No migration. The golden encoding test
  is unaffected (it encodes an `Op`).
- **Older binaries** reading a repository with stacked or permanent lanes
  would ignore the new fields and take every checkpoint for main's. The
  store's format version is raised (`kito-store 3`) the first time a
  lane with a parent, a target or a name is written, so that an older
  binary refuses the store instead of misreading it.
- **Harder**: what a lane "is based on" is no longer one checkpoint of
  main but a chain; the tests that only had main and lanes gain cases with
  depth. Rollback on a permanent lane rolls back its checkpoints only,
  and children of a lane are moved when it is rolled back below their
  `base_head`, as when it closes.
- **Not done here**: lanes that are not permanent landing on each other
  in both directions (catch-up between two open lanes), more than one
  repository-level main, and review (diffs, comments, approvals), which is
  the next phase.

## Plan

Two phases, each closed as usual:

- **Phase 15 · Lanes on lanes**: the fields, `state_at` through parents,
  stacked lanes, targets, moving children when a parent closes, landings
  on a lane served without waiting for the main lane's queue; CLI and MCP
  (`--on`, `--target`); why graph through parents.
- **Phase 16 · Permanent lanes**: names, checkpoints and git branches per
  permanent lane, `integrate staging` and `catch-up`, checks and
  protection per lane, push and pull per branch, `integrate` lists on the
  server.

Review of a lane with a web view moves to phase 17.
