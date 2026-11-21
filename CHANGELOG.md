# Changelog

Each version is one phase of the project; [docs/phases.md](docs/phases.md)
says what each built, how it was tested, and its limits. The release notes
of a version are its section here.

## [Unreleased]

Review of a lane ([ADR 0004](docs/adr/0004-review-of-a-lane.md)), under way:

- `kito lane diff <lane> [path…] [--stat]`, and the MCP tool `lane_diff`:
  what a lane changes against what it started from. Per file its lines,
  and the functions it added, changed, removed or renamed, each with the
  intents of the lane's ops on it. A function renamed or moved is one
  change, with the name it had. On a server, anybody who may read may ask.
- Comments: `kito lane comment <lane> "<text>"` (`--on src/shop.ts:12-14`,
  `--reply comment#3`), `kito lane comments`, `kito lane resolve-comment`,
  and the MCP tools `comment`, `comments`, `resolve_comment`. A comment on
  lines stays on them while the lane has them, and is marked once it does
  not. The agent in the lane is told the comments before its next edit,
  once; `kito lane status` shows the ones that wait. On a server,
  commenting needs `write`.

Windows ([ADR 0002](docs/adr/0002-windows.md)):

- Kito runs on Windows itself, not only in WSL: its tests pass there and
  run in CI next to Linux and macOS.
- A release has `kito-<version>-windows-x86_64.zip`, and `install.ps1`
  installs it (`irm .../install.ps1 | iex`).
- Not on Windows: the sandbox of checks and lane commands (they run
  unconfined, and Kito says so), the executable bit, and links, which are
  kept as files holding their target.
- Checks run through `sh` when Git for Windows' is on the `PATH`, and
  through `cmd` otherwise.
- Fixed there: the store's recovery truncates through a handle that may
  (a file opened to append may not); server tokens come from the system's
  randomness instead of `/dev/urandom`.

## [0.18.0]

The why graph, better (phase 18; phase 17, review, comes after):

- An intent that says nothing (`wip`, `fix`, `update`, `minor changes`) is
  refused when a lane is opened or an intent is set: it is what the next
  agent is told before it changes the code. Intents Kito writes itself
  (git commit subjects, integrations) are taken as they are.
- Notes on code ([ADR 0003](docs/adr/0003-notes-on-code.md)): `kito note
  <target> "<text>"`, `--list`, `--remove`, and the MCP tool `note`. A note
  on a function follows it through renames and moves; notes on a file or
  a directory are told for what is in it. They are shown with the why of
  that code (`annotations`), and an agent about to change it is told them
  once, with the reasons.
- A renamed file keeps the reasons of its lines: its history is followed
  under the names it had before. A new file given the name of an earlier
  one no longer takes that one's history.

## [0.16.0]

Permanent lanes ([ADR 0001](docs/adr/0001-lanes-on-lanes.md)).

- `kito lane create staging`: a lane next to `main` that is never closed,
  named wherever a lane is named (`--on staging`, `kito integrate staging`).
- Landing on a permanent lane makes a checkpoint of it, runs its checks
  (`[lanes.<name>] verify`), and exports it to its own git branch, pushed
  with main's.
- `kito integrate staging` lands it on main as a git merge; it stays open.
- `kito lane catch-up staging` brings in what landed on main directly.
- `[lanes.<name>] integrate`: who may land on it.
- The store's format becomes `kito-store 3` once a permanent lane or a lane
  on a lane exists; older versions refuse it.
- Fixes: Kito asked rust-analyzer before it had loaded the project, between
  two of its phases of work, and could find no use of a function; it now
  waits until rust-analyzer says it is done. A file with syntax errors could
  be read differently depending on its previous version; it is now read
  from scratch.
- Tested on macOS in CI for the first time. Known limit there: a save that
  a program writes slowly, in several steps, may be recorded half-way, then
  again when complete. Linux knows a file is complete when it is closed
  after writing; macOS's file events do not say so. Editors that save
  quickly or by renaming a new file into place are not affected.

## [0.15.0]

Lanes on lanes ([ADR 0001](docs/adr/0001-lanes-on-lanes.md)).

- `kito lane open --on <lane> [--target <lane>]`: a lane on work that has
  not landed, landing on it, or further down once that work has landed.
- Landing on a lane makes no checkpoint and runs no checks, and does not
  wait behind the main lane's queue.
- The why graph follows lines through the lanes they came through.
- Fixes: a lane command that timed out killed every process of the user on
  Ubuntu 24.04 (procps-ng 4.0.4's `kill`); a store reopened at once could
  find its lock held by a forked child.

## [0.14.0]

Git both ways: commits made with plain git on the remote are brought in
through a lane, merged, and pushed back as a git merge.

## [0.13.0]

Integration without waiting in line: lanes that do not depend on each other
are checked in one run.

## [0.12.0]

Encrypted connections and a sandbox per lane.

## [0.11.0]

The rules as behaviour: hooks that refuse an edit outside a lane or its
scope, and the reasons told before code changes.

## [0.10.0]

The why graph: the reasons of every function, file and line.

## [0.9.0]

Protected main and team rules: roles, approval, checks and git push on the
server.

## [0.8.0]

Lanes from any machine: one writes, others follow, a journal of saves.

## [0.7.0]

One server for several repositories, clients on other machines.

## [0.6.0]

The intent board: declared scope, overlaps between lanes.

## [0.5.0]

Coverage and performance; then 0.5.1 (ready for Claude Code from
`kito init`) and 0.5.2 (a shorter `kito log`).

## [0.4.0]

Level 2 semantics: language servers, `SemanticRef` conflicts.

## [0.3.0]

Lanes and integration, the MCP server.

## [0.2.0]

Level 1 semantics: tree-sitter, stable IDs, `kito why`.

## [0.1.0]

Continuous capture, the daemon, the CLI, git export and import.
