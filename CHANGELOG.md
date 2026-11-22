# Changelog

Each version is one phase of the project; [docs/phases.md](docs/phases.md)
says what each built, how it was tested, and its limits. The release notes
of a version are its section here.

## [0.39.0]

Plugins of a server that work on their own
([ADR 0020](docs/adr/0020-plugins-that-work-on-their-own.md)).

- A plugin that serves pages asks `/-/api` as itself, with the secret
  the server gave it and what it was given; the server also listens on
  127.0.0.1 for its plugins (`KITO_SERVER_API`).
- Pages are told the person's role on the server (`x-kito-role`).
- **`git_sync`**: what the git remote has comes in, and the main lane is
  pushed, now.

## [0.38.0]

A review sent whole ([ADR 0019](docs/adr/0019-a-review-sent-whole.md)).

- **`submit_review`**: the comments of a review and its verdict in one
  go, every comment checked before any is written; the agent is told
  them together. A request for changes that stands is told to the agent
  too.
- **Suggested changes**: a comment on lines carries the lines to put in
  their place; the agent is told them, and `apply_suggestion` writes them
  into the lane and resolves the comment.
- **What changed since my review**: a verdict keeps what the files were;
  `changes_since_review` shows what changed since, against that.

## [0.37.0]

- **The command line has Kito's look**: `kito --help` starts with the
  logo (three lanes that become one line, ending in a dot), the name and
  the version, and lists the commands in groups, the most used first
  (start, lanes, history, assistants and plugins, git, server, settings).
  Headings are in Kito's indigo; commands and flags in bold; lane and
  checkpoint names, what went in, notes and the `kito:` of errors are
  coloured too. Colour is only for a terminal: none when the output goes
  to a file or a program, with `NO_COLOR`, with `TERM=dumb`, or with
  `--json`; `CLICOLOR_FORCE=1` asks for it anyway (D-174).
- Help lines are shorter: the first sentence is shown with `-h`, the rest
  with `--help`. `kito hook` is for assistants, and is no longer listed.
- **`kito claude` is gone**: `kito assistant add claude` does the same.

## [0.36.2]

- **A plugin enabled on a server works at once**: its pages are served,
  and taken away when it is disabled, without starting the server again.
- The web client is told which plugins run on the server (`/-/me`), to
  show only what belongs to them.

## [0.36.1]

- **A plugin runs where it is meant to**: the registry says, for each
  plugin, whether it runs on a server, in a repository, or both.
  `kito-server plugin add` and `enable` refuse a plugin that sets up a
  coding assistant (`claude`, `codex`, `zcode`: for the machines people
  work on), and `kito plugin add` and `enable` one that is only for a
  server (`oidc`), saying where each goes.

## [0.36.0]

Repositories from afar (phase 35,
[ADR 0018](docs/adr/0018-repositories-from-afar.md)).

- **`kito repos add`** for admins in a connected directory: an empty
  repository, one cloned from a git address (a token used once and kept
  nowhere), or one sent from this machine with `--push`: a Kito
  repository with all its history, or a git one.
- **`kito repos archive`**: out of use, deleted after 30 days; owners
  `unarchive` or `delete` it now. **`kito repos rename`**: directories
  connected with the old name still reach it.
- The same on the server's machine: `kito-server repo archive |
  unarchive | delete | rename`.

## [0.35.0]

The server's audit log and backups (phase 34,
[ADR 0017](docs/adr/0017-audit-and-backups.md)).

- **`kito-server audit`**: who changed the server's people, access,
  rules, plugins and repositories, every sign-in, and every request (90
  days); each line chained to the one before, `--verify` finds a change.
- **`kito-server backup`**: the whole server in one encrypted archive,
  made while it runs, every 24 hours by default with the last 14 kept,
  in a folder and in S3-compatible storage; `kito-server restore` puts
  one back.
- `kito audit` and `kito backup` for admins in a connected directory.

## [0.34.0]

Search and numbers (phase 33, [ADR 0016](docs/adr/0016-search-and-numbers.md)).

- **`kito search`**: reasons, changes, lanes, tasks, comments and notes,
  by words, kind, time, who (a person, an agent, an assistant, a
  model), where (a file, a directory, a function) and outcome.
- **`kito stats`**: lanes week by week and how long they take, each
  assistant and model, the busiest code, review.
- `--everywhere` asks every repository you may use on the server; agents
  have the tool `search`.
- What happens is kept from now on (`.kito/activity.jsonl`), refusals
  and held edits included.

## [0.33.0]

Tasks (phase 32, [ADR 0015](docs/adr/0015-tasks.md)).

- **`kito task`**: ask for something to be done, say whom it is for,
  comment on it, close it. Kept by Kito, shared through the server, never
  in the project's files.
- **Lanes do tasks**: `kito lane open --task task#3`; a task has as many
  lanes as it needs, and is ready to close once they are all closed.
- **Agents take tasks up when told** (`tasks`, `claim_task`), hear what
  is said on them, and say when they think one is done; a person closes
  it.
- The `kito connect` line leaves out the fingerprint of a server whose
  certificate comes from an authority.

## [0.32.0]

Instructions, for the web client (phase 31).

- **What an assistant reads**, Kito's rules first, asked of the daemon
  (`instructions_text`): the same text `kito instructions show` prints.
- **Who changed which instruction**, and the text before and after,
  kept for each repository (`instructions_history`).

## [0.31.0]

A repository's rules, for the web client (phase 30).

- **The checks are changed by the server's admins** only, through the
  server: they run commands on its machine.
- **A kitchen** given through the server is another of its repositories.
- **Who changed which rule**, and to what, is kept and shown to
  maintainers and admins.

## [0.30.0]

People, for the web client (phase 29).

- A role changed, or a repository taken away, counts at once in open
  connections.
- When each person was last here, given with the members and the
  people.
- The server's people over HTTP (`/-/people`), and who uses a
  repository through `/-/api/<repository>`, for kito-web 0.11's people
  pages. `kito members` is for maintainers and admins.

## [0.29.0]

Roles per repository, and roles on the server (phase 28,
[ADR 0014](docs/adr/0014-roles.md)).

- **A role in each repository**: `read`, `write`, `integrate`, and the
  new `maintain`, who changes its rules and says who uses it.
  `kito-server grant <name> <repo> <role>` and `revoke`; `user add
  --repo shop:write`.
- **A role on the server**: `member`, `admin`, `owner`
  (`kito-server user add --role`, `user role`). `user admin` is gone.
- **From a connected directory**: `kito members`, `kito grant`,
  `kito revoke`, and `kito people` for admins.
- A person added with no repository may use none until granted one; an
  existing server is read as it was meant.

## [0.28.0]

Connecting as GitHub's tools do (phase 27,
[ADR 0013](docs/adr/0013-connect-by-address.md)).

- **`kito connect kito.example.com/shop`**: one address; `kito clone` is
  the same command.
- **Signing in in the browser**: with no token, Kito asks the server how
  to sign in, lets you pick (or paste a token) and opens the page.
- **Who the server is**: a public certificate needs no flag; a self-made
  one is shown once at the terminal, and kept when you say it is yours.

## [0.27.0]

What agents are told is kept by Kito and shared, not part of the
repository (phase 26, [ADR 0012](docs/adr/0012-instructions.md)).

- **Instructions**, one standard: `all/<name>.md` for every assistant,
  `<assistant>/<name>.md` for one. A kitchen attached to the repository
  gives its own first; the repository's win.
- **`kito instructions`** (`show`, `set`, `remove`, `kitchen`, `write`).
  In a directory connected to a server they are the server's, for
  everyone; `kito-server repo set <repo> --kitchen <repo>`.
- **`CLAUDE.md` and `AGENTS.md` are written by Kito** from them, at
  every session. What they said before becomes an instruction.
- **The files assistants read stay out of the history and of git** by
  default, and `.gitignore` names them. `[agents] files_in_history =
  true` keeps them recorded.

## [0.26.0]

One way to add any assistant, and the ones a machine has (phase 25,
[ADR 0011](docs/adr/0011-one-way-to-add-an-assistant.md)).

- **`kito assistant add <name>…`** sets up any assistant with the same
  command: `claude` by Kito itself, the others by their plugin, which is
  installed from the registry and enabled here when needed. Each step is
  said. `kito assistant remove claude` now takes Claude Code's files out.
- **`kito assistant list`** says which assistants this machine has and
  how each is set up; **`kito assistant default`** says once which ones
  every new project gets.
- **`kito init`, `import` and `connect`** set those up. With nothing said
  they ask at a terminal for each assistant found; with nobody to ask
  they set up Claude Code, as before, and name the others.
  `--assistant <name>` and `--no-assistants` say it for one project.
- The install script asks the same question once.

## [0.25.0]

A lane says which assistant works in it, and on what model (phase 24,
[ADR 0010](docs/adr/0010-assistant-in-a-lane.md)).

- **`assistant` on a lane**: a name (`claude-code`), and a version and a
  model when known. In `lanes`, `lane_status`, `board` and the answer of
  `open_lane`; `kito lanes` and `kito board` say it.
- **Through the MCP server** it is the name the program gave when it
  connected, with nothing to set up, and the model if the agent gives
  `open_lane` one (`model`, optional).
- **Through the hooks**: a session says what it runs on when it starts
  (Claude Code gives its model then), and the lane its next edit is in
  keeps it. `kito hook before-edit` and `session-start` take
  `--assistant` for the hooks of other assistants; `before-edit` takes
  `--model` and `--session` too. Claude Code is guessed from the mark it
  leaves in the environment: a guess names a lane nobody named, and
  never replaces a name a program gave of itself.
- The MCP server says which program asks at every question about a lane:
  a lane taken over by another assistant says so.
- **`kito lane open --assistant NAME [--model M]`**, by hand.
- The API stays version 2: `open_lane`, `before_edit` and `notices`
  take `assistant`, `before_edit` takes `session`, and
  `assistant_session` is new (needs `read`, records nothing).
- It is a label: what the assistant's side said of itself, cleaned
  before it is kept, and by which nothing is decided.

## [0.24.0]

The registry is signed (phase 23,
[ADR 0009](docs/adr/0009-signed-registry.md)): who holds a registry's
key decides what a name installs, and nobody who serves or carries its
files does.

- **A registry's files are signed** with its key (Ed25519), a signature
  beside each (`index.json.sig`). With the key known, a file that is not
  signed by it, or changed since, is refused, and nothing of that
  registry is used.
- **`KITO_REGISTRY_KEY`** says the key of a registry (several, with
  commas: one is enough). The key of the registry at kito.sh is to be
  written in Kito itself; it is not made yet, and that registry is taken
  as its address gives it until it is. `plugin add` says whether the
  registry was checked.
- **A signed index does not go back**: it is numbered, and one numbered
  lower than this machine saw before is refused. A plugin's file must
  have the version the index says is the latest.
- **`kito registry key <file>`** makes a key; **`kito registry sign
  <directory> --key <file>`** signs a registry kept in a directory.
- `/-/admin/plugins` says whether the server's registry is checked
  (`registry_signed`).
- Outside this repository: a kitchen's tools run confined to the lane,
  as `kito lane run` runs a command (`kito-kitchen` 0.3); issues at
  GitHub as the tasks of lanes (`kito-github` 0.2).

## [0.23.0]

A registry of plugins (phase 22,
[ADR 0008](docs/adr/0008-plugin-registry.md)): a plugin is installed by
its name, taken out as cleanly as it went in, and a server's plugins are
changed from a browser by its administrators.

- **`kito plugin add <name>`** (and `<name>@<version>`; the same for
  `kito-server plugin add`): the plugin is downloaded from what the
  registry lists, checked against the registry's SHA-256, unpacked
  apart, and installed only if the manifest inside is the plugin and the
  version asked for. `--path` stays.
- **`kito plugin search [words]`**: what the registry has.
- The registry is `https://kito.sh/registry`, or the one `KITO_REGISTRY`
  names: a team's own, or files on this machine (`file://`).
- Nothing is published yet: the registry at kito.sh is empty.
- **Taking a plugin out**: a plugin with `[cleanup]` in its manifest is
  run one last time where it goes. `kito plugin disable <name>` (and
  `kito-server plugin disable`) runs `kito-<name> cleanup repository`
  (or `server`): what it wrote there comes out with it. Its key is
  removed; what it kept of its own stays, unless `--purge`. `kito plugin
  remove <name>` runs `cleanup person` first. A cleanup that fails is
  said, and the plugin goes all the same; `--no-cleanup` does not run it.
- **`kito-server plugin remove <name>`**: off the server's machine, once
  it is disabled.
- **`kito plugin outdated`** and **`kito plugin update [<name>]`**: the
  plugins the registry has a later version of that fits this Kito and
  this machine, and that version installed.
- **Administrators of a server**: `kito-server user admin <name>`
  (`--off`), said on the server's machine only. `/-/me` says who is one.
- **`/-/admin/plugins`** on the server's HTTP address, for
  administrators: what is installed, enabled and with what, and what the
  registry lists; install (by name, from the server's registry, and
  nothing else), enable (with the permissions given, `for_clients`),
  disable (`purge`), remove. The plugin that serves the pages is not
  taken out from them. Every change is told as a server event
  (`plugin_installed`, `plugin_enabled`, `plugin_disabled`,
  `plugin_removed`), with who made it. The page is in `kito-web` 0.5.
- Fixed in the tests: the hooks' test said a simulated command took 40
  ms and took longer itself on a busy machine, so Kito rightly did not
  blame the command; it now writes again until the hook is that quick.
  Where the same test once let an edit through that should have been
  held back, it now says what Kito knew of those lines: not seen again,
  and not explained.

## [0.22.0]

A team's kitchen (phase 21, [ADR 0007](docs/adr/0007-kitchens.md)): what
Kito needs so that a plugin can give every agent of a team the same
behaviour, concepts, recipes and tools, kept on the team's server.

- **Plugins run in a directory connected to a server**: those the
  server enabled for its clients (`kito-server plugin enable <name>
  --for-clients`), with what of a repository the server gave them, for
  whoever has them installed. A person enables nothing there.
- **`plugin_call`**: a plugin asks the plugin of its own name on the
  server, which runs `kito-<name> answer <question>` and is told who
  asks, about which repository and with which role. The daemon's API is
  version 2; plugins written for version 1 run as they did.
- **What a plugin says when a session starts**: a plugin with
  `[session]` in its manifest is run then, and what it prints is given
  to the agent after Kito's own rules, under whose words they are.
- The kitchen itself is the plugin `kito-kitchen`: a repository on the
  server (`behaviour.md`, `concepts/`, `recipes/`, `tools/`, and
  `parts/` a repository picks from), `kito-server kitchen …` there,
  `kito kitchen …` and tools for agents in every connected directory,
  and a copy for when the server is away.

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
