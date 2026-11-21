# Changelog

Each version is one phase of the project; [docs/phases.md](docs/phases.md)
says what each built, how it was tested, and its limits. The release notes
of a version are its section here.

## [0.21.0]

Plugins for the server (phase 20,
[ADR 0006](docs/adr/0006-server-plugins.md)): commands, events, pages
and signing in are added to a server by programs of their own.

- `kito-server plugin add --path <dir>`, `enable <name>` (`--only …`),
  `disable`, `list`: the same plugins, enabled by a server in
  `server.toml`. What a server gives: `sign_in`, `pages`,
  `server_events`, `server_admin`.
- **Commands**: a plugin's `[[server_command]]` is a command of
  `kito-server` on the servers that enabled it, with the server's
  directory in `KITO_SERVER_DIR`.
- **The server's events**: a plugin's `[[server_event]]` lists kinds
  (`repo_added`, `repo_changed`, `user_added`, `user_removed`,
  `role_changed`, `plugin_failed`); it is run for each, after the fact.
  One that fails is said where the administrator is and in `plugin
  list`, and undoes nothing.
- A repository gives a plugin only what is a repository's to give
  (`read`, `write`, `integrate`, `admin`).
- A command of a plugin replaces `kito` on Unix: stopping `kito web`
  stops the web client. Plugins are told where the daemon listens
  (`KITO_SOCKET`).
- Fixed: after a shell command, an agent could be told that the
  command changed files of the main directory that were written before
  it and only recorded while it ran (a busy machine). An edit by hand is
  now dated by its files as well as by when it was recorded.
- **The server's HTTP address**: `kito-server serve --http <address>`
  (or `http = …` in `server.toml`), encrypted with the server's
  certificate. `/-/me` says who asks; `POST /-/session` with a token
  opens a session kept in a cookie; `POST /-/api/<repository>` asks that
  repository's daemon as the person, within their role.
- **Pages**: every other path is passed on to the plugin whose
  `[pages]` are mounted there, which the server starts and keeps
  running. It is told who asks (`X-Kito-User`), never handed their token
  or their session. What pages ask of a repository is held to what the
  plugin was given.
- **Signing in**: a plugin with `[sign_in]` says who somebody is, and
  the server says what they may do. `kito-server user add bob --identity
  github:bob` adds a person who has no token; the plugin resolves the
  login, and the server keeps what does not change when it is renamed.
- `kito connect <server> <repo> --sign-in <method>` shows where to go
  and the code to enter, waits, and keeps a session; `kito sign-in` gets
  another when it has ended. A session lasts `[sign_in] days` (30), is
  kept by the server as a hash, outlives a restart, and ends with the
  person.
- In a browser, `/-/sign-in` lists the ways, `/-/sign-in/<method>` goes
  to the provider and comes back to a session cookie. The server's
  events gained `signed_in`, `sign_in_refused` and `session_ended`.
- Not in this version: a plugin that signs in with a real provider
  (`kito-github` is next), and signing in for `ssh` sessions.

## [0.20.0]

Plugins (phase 19, [ADR 0005](docs/adr/0005-plugins.md)): what does not
belong in Kito is added by programs of their own.

- **A plugin is a program**, `kito-<name>`, with a `kito-plugin.toml`.
  `kito plugin add --path <dir>` installs one for you; `kito plugin enable
  <name>` lets it run in a repository, with the permissions it asks for
  (`--only read,write` gives less); `list`, `info`, `disable`, `remove`.
  Nothing of a plugin runs in a repository that did not enable it.
- **Permissions**: the daemon knows a plugin by a key of its own and
  answers it what the repository gave it (`read`, `write`, `integrate`,
  `admin`), refusing the rest and naming the permission that is missing.
- **The daemon's API**, version 1, is written down in
  [docs/api.md](docs/api.md); `ping` answers `api`.
- **Commands**: a command a plugin declares is a command of Kito in the
  repositories that enabled it (`kito web`). It is run with `KITO_REPO`,
  `KITO_API`, `KITO_SOCKET` and `KITO_PLUGIN_KEY` in its environment.
- **Gates**: a plugin's `[[gate]]` is asked before a lane is integrated,
  after the checks, in the same confined copy: it answers pass, fail
  (the lane's checks failed, naming the gate) or wait (the lane waits,
  with the gate's reason, and is asked again). No answer in time is a
  failure.
- **Events**: a plugin's `[[event]]` lists kinds; it is run for each,
  one at a time and in order, after the fact. What a plugin last failed
  at is in `kito status` and `kito plugin list`, and told as
  `plugin_failed`.
- **Tools for agents**: a plugin's `[[tool]]` is offered by `kito-mcp`
  as `kito.<plugin>.<tool>`, after Kito's own and saying whose it is.
- **Assistants**: a plugin with `[assistant]` sets a coding assistant up
  for Kito: `kito assistant add | remove | list`. Claude Code is still
  set up by Kito itself.
- An integration that waits says `it waits: …` (for its review, or for a
  gate). Events name their lane `lane/1` however it was typed. `kito`
  runs on a thread with a stack of its own (it overflowed the main
  thread's on Windows).
- Not yet: installing a plugin by name from a release; plugins in a
  directory connected to a server.

## [0.19.0]

Review of a lane (phase 17, [ADR 0004](docs/adr/0004-review-of-a-lane.md)).
Kito has no interface for it: it is data in the store, and questions the
daemon and the server answer.

- **The diff of a lane**: `kito lane diff <lane> [path…] [--stat]`, and
  the MCP tool `lane_diff`: what a lane changes against what it started
  from. Per file its lines, and the functions it added, changed, removed
  or renamed, each with the intents of the lane's ops on it. A function
  renamed or moved is one change, with the name it had.
- **Comments**: `kito lane comment <lane> "<text>"` (`--on
  src/shop.ts:12-14`, `--reply comment#3`), `kito lane comments`, `kito
  lane resolve-comment`, and the MCP tools `comment`, `comments`,
  `resolve_comment`. A comment on lines stays on them while the lane has
  them, and is marked once it does not. The agent in the lane is told the
  comments before its next edit, once; `kito lane status` shows the ones
  that wait.
- **Verdicts**: `kito lane approve`, `request-changes`, `withdraw` and
  `review`, and the MCP tool `review`. An approval is of the lane as it
  was: it is stale once the lane changes. A request for changes stops the
  lane until it is taken back or replaced. The owner's own approval does
  not count.
- **What integration asks**: `[review] required = N` (`agents`, `keep`;
  `required` under `[lanes.<name>]` for what lands on a permanent lane;
  `kito-server repo set <repo> --approvals N` on a server). A lane that
  lacks its review is merged, checked and then waits, saying what is
  missing. Approving brings nothing in: `kito integrate` does, for
  whoever may. With `required = 0`, the default, nothing changes.
- Who reviewed is on the checkpoint, in the why of that code, and in the
  exported commit (`Reviewed-by:`).
- On a server: reading a diff and a review needs `read`; commenting and
  giving a verdict need `write`.

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
