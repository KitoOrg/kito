# Changelog

Each version is one phase of the project; [docs/phases.md](docs/phases.md)
says what each built, how it was tested, and its limits. The release notes
of a version are its section here.

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
