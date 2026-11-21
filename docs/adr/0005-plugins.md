# 0005 · Plugins

Status: accepted
Date: 2026-11-21

## Context

Kito is the store, the daemon, the command line, the MCP server and the
server for teams. Several things people ask for do not belong in it:

- **An interface.** Review (ADR 0004) is data and questions; a web view
  is another program, and Kito has none built in (decided with the person
  on 2026-11-20).
- **Other coding assistants.** `kito init` sets up Claude Code: its MCP
  configuration, its rules, its hooks. Codex, Cursor, Gemini CLI and the
  next one each want the same three things in their own files and with
  their own hook events.
- **What a team adds to integration**: a licence check, a ticket that must
  exist, a message to a channel when a lane lands.

Today each of these means changing Kito. Programs outside Kito can
already ask the daemon everything `kito` asks (JSON-RPC, one message a
line, over the repository's socket), but nothing says which of them a
repository relies on, what each may do, or that the questions will still
be answered the same way next version.

This changes one of the plan's choices (the interfaces of Kito), which
the ADR rules ask to write down first.

## Decision

### 1. A plugin is a program

A plugin is an executable named `kito-<name>` with a manifest. It is not
loaded into the daemon: it is a client of the daemon, as `kito`,
`kito-mcp` and `kito-server` are. It can be written in any language; when
it crashes, nothing of Kito's does; no code of a plugin runs inside the
process that holds the store.

```toml
# kito-plugin.toml, next to the executable
name = "web"
version = "0.1.0"
description = "Kito in a browser: the board, review, the why graph"
api = 1                              # the daemon API it was written for
permissions = ["read", "write", "integrate"]

[[command]]                          # `kito web [...]` runs `kito-web [...]`
name = "web"
about = "Serve the web client for this repository"

[[event]]                            # run for these events, one at a time
kinds = ["integrated", "commented"]

[[gate]]                             # asked before a lane is integrated
name = "licence"
about = "Every new file has a licence header"
timeout_secs = 60

[[tool]]                             # offered to agents by kito-mcp
name = "open"
description = "The address of a lane in the web client"
input = { type = "object", properties = { lane = { type = "string" } }, required = ["lane"] }

[assistant]                          # sets up a coding assistant (section 6)
name = "codex"
```

Every section but the first lines is optional: a plugin provides any of
commands, events, gates, tools, an assistant.

### 2. Installed for a person, enabled by a repository

- **Installed** in the person's Kito directory, `plugins/<name>/` under
  `KITO_CONFIG_DIR`: `kito plugin add <name>` for a plugin Kito knows by
  name (`web` is `KitoOrg/kito-web`), `kito plugin add <owner>/<repo>`
  for any other, `kito plugin add --path <dir>` for one built here. From a
  GitHub release, the archive for the platform, checked against its
  SHA-256 as `install.sh` does. `kito plugin list`, `info`, `remove`,
  `update`.
- **Enabled** by a repository, in `.kito/config.toml`:

  ```toml
  [plugins.web]
  enabled = true
  permissions = ["read", "write", "integrate"]   # what it was given
  ```

  `kito plugin enable web` shows what the plugin asks for and what it
  provides, and writes that; `kito plugin disable web`. **Nothing of a
  plugin runs in a repository that did not enable it**, and an installed
  plugin that a repository enables but this person does not have is said,
  with the command that gets it.
- A plugin whose manifest asks for more than the repository gave it runs
  with what it was given. When a new version asks for more, it keeps what
  it had until somebody enables it again.
- A registry (an index of plugins by name, with versions and checksums)
  is not part of this: it comes when there are several plugins to index.

### 3. What a plugin may ask: the daemon API, versioned

The API of plugins is the daemon's JSON-RPC, which becomes a documented
interface with a number.

- `ping` answers `api`: the version the daemon speaks. Version 1 is the
  methods of Kito 0.19, written down in `docs/api.md` with their
  parameters and answers.
- Adding a method, or an optional field to an answer, does not change the
  number. Removing one, or changing what it means, does; the daemon then
  answers the version before as well, for one more release.
- A plugin written for a newer API than the daemon speaks is not run, and
  Kito says which of the two to update.

**Permissions** are groups of methods, as a server's roles already are
(D-094):

| Permission | May ask |
| --- | --- |
| `read` | status, lanes, board, log, why, diffs, comments, reviews, notes, events, contents |
| `write` | open and close lanes, save files in them, set intents, extend scopes, comment, give verdicts, leave notes |
| `integrate` | ask for integrations, resolve conflicts, catch up |
| `admin` | protect, configure, checkpoint, rollback |

Kito starts a plugin with what it needs to reach the daemon in its
environment: `KITO_REPO`, `KITO_API`, and `KITO_PLUGIN_KEY`, a key the
daemon made for this plugin in this repository. The daemon answers that
key only what the repository gave the plugin, and refuses the rest with
the name of the permission that is missing. The key of the socket, which
`kito` itself uses, is not given to plugins.

On a server a plugin acts **as the person** it runs for: what it may do
is what its permissions and that person's role both allow, never more.

### 4. What a plugin provides

- **Commands.** `kito <name> [...]` for a name Kito does not have runs
  the enabled plugin that declares it, in the repository, with the rest
  of the line; its exit code is `kito`'s. Kito's own commands cannot be
  taken. `kito help` lists them apart, with the plugin each comes from.
- **Events.** For the kinds it lists, the daemon runs `kito-<name> event`
  with the event as JSON on its input: one at a time and in order for
  each plugin, after the event happened, never holding up what caused
  it. One that fails or runs past its time is said in `kito status`, and
  the next event is still delivered. A plugin that stays up (a web
  client) asks `events` itself instead, as `kito watch` does.
- **Gates.** After the merge and the checks of an integration, and before
  the review is asked (ADR 0004), the daemon runs each gate: `kito-<name>
  gate <gate>`, in the copy of the merged result, with the lane, its
  target and its diff as JSON on its input. It answers:

  ```json
  { "result": "pass" }
  { "result": "fail", "message": "src/new.ts has no licence header" }
  { "result": "wait", "message": "the ticket SHOP-12 is not approved yet" }
  ```

  `fail` is a check that failed (`verification_failed`, naming the gate);
  `wait` leaves the lane `awaiting_approval` with that message as what is
  missing, and the gate is asked again when integration is. No answer in
  its time, or one that cannot be read, is `fail`, saying so. Gates run
  confined like the checks (phase 12): they write only in their copy.
  On a server the gates are the repository's, like its checks: no client
  adds or removes one.
- **Tools for agents.** `kito-mcp` offers the tools of the plugins the
  repository enabled, named `kito.<plugin>.<tool>`, so that an agent and
  whoever reads its transcript see where each comes from. A call runs
  `kito-<name> tool <tool>` with the arguments as JSON on its input; what
  it prints is the answer. A tool has its plugin's permissions.

A plugin keeps what is its own under `.kito/plugins/<name>/`. It writes
nothing else in `.kito`, and adds no kind of record to the store.

### 5. What Kito says about plugins

`kito status` lists the plugins enabled, their versions, and what each
last failed at. Events `plugin_failed` and `gate_failed` for whoever
watches. A gate's verdict is in the verification of the checkpoint, with
the checks.

### 6. Assistants

A plugin with `[assistant]` sets a coding assistant up for Kito:

- `kito-<name> assistant setup` writes that assistant's files in the
  repository (its MCP configuration with `kito-mcp`, its rules, its
  hooks), and `assistant remove` takes them out; `kito init --assistant
  <name>` and `kito assistant add <name>` run it.
- Its hooks are its own programs: they read what the assistant sends
  before an edit, after a command and when a session starts, ask Kito
  (`kito hook before-edit`, `after-shell`, `session-start`, which stay
  the one implementation of the rules), and answer in that assistant's
  words.
- `assistant setup` says what that assistant cannot be held to: one with
  no hook before an edit gets the tools and the rules, and its edits
  outside a lane are caught when they are saved (a protected main lane
  puts them back), not before.

Claude Code stays set up by Kito itself until a second assistant has
proved the shape; then it moves to a plugin like the others.

## Consequences

- **The daemon's methods are a promise.** Until now they changed with
  `kito`, in the same commit. From API 1 on, a change that breaks them
  needs a new number and a release that speaks both.
- A repository without `[plugins]` behaves as before. An older Kito
  ignores the section.
- A gate can stop every integration of a repository. That is the
  repository's choice, made by whoever may configure it; `kito plugin
  disable` undoes it, and a gate that fails says which plugin it is.
- Plugins are trusted as programs the person installed: Kito checks the
  download and limits what the daemon answers them, and does not confine
  what a command or an event handler does on the machine. Only gates run
  confined.
- A plugin for each platform is its author's to build. A plugin with no
  build for a platform is said so by `kito plugin add`.
- Not done here: plugins loaded into the daemon (WebAssembly or native);
  a registry; plugins that add records to the store or screens to another
  plugin; signatures on plugins beyond the checksum of the release.

## The first plugins

Not part of Kito's repository, and not decided by this ADR beyond what
they need of it:

- **`web`** (`KitoOrg/kito-web`): one program that serves a web client
  it carries inside (a Rust server, a Svelte frontend built at release).
  `kito web` serves it on this machine for whoever runs the repository;
  next to a `kito-server` it serves a team, who sign in with the server's
  tokens and act within their roles. In this order: the board, a lane's
  diff, comments and verdicts; the why graph, the log and notes; opening,
  closing and integrating lanes, and conflicts; signing in and the
  server's administration.
- **Assistants**: `codex` first, then the others, each after checking
  what hooks that assistant has.

## Order of work

One phase (19), released as 0.20.0, in three parts, each with its tests
and one commit:

1. the API written down and numbered, plugin keys and permissions, the
   manifest, `kito plugin add | enable | list | …`, and commands;
2. events and gates;
3. tools for agents, assistants, the demo (a plugin written for it) and
   the documents.

The web client starts when part 1 is done: it needs commands,
permissions and the API, and nothing of parts 2 and 3.
