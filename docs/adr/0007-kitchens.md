# 0007 · A team's kitchen

Status: accepted
Date: 2026-11-21

## Context

Kito tells an agent the rules of version control here (work in a lane,
change what was declared, know why). It tells it nothing of how this
*team* works: which commands build and test, what the words of the
domain mean, how an endpoint is added, which tools everybody has. Today
that lives in each repository's `CLAUDE.md` or `AGENTS.md`, copied by
hand, different in every repository and for every assistant, and stale
in most.

A **kitchen** is what a team shares to work on its repositories: one
place for its behaviour, its concepts, its recipes and its tools, the
same for every agent of every person, whatever assistant they use.
Decided with the person on 2026-11-21: it lives on the Kito server; it
holds behaviour, concepts, tools and recipes; a repository gets the
whole kitchen and picks optional parts on top; it is a plugin.

Two things it needs are not in Kito, and are an interface:

- **Plugins do not run in a directory connected to a server** (a limit
  of ADR 0005), and that is where a team's agents work.
- **A plugin on a person's machine cannot ask a plugin of the server.**
  The server passes on a fixed list of the daemon's methods, and pages
  over HTTP, which a command line does not reach.

## Decision

### 1. A kitchen is a repository on the server

The kitchen is kept as a Kito repository the server hosts, named by the
server (`kito-server kitchen set <repository>`): it has a history,
changes to it go through lanes and review like any code, and who may
change it is a role in that repository. No new store.

```text
kitchen.toml            what the kitchen is called, and its parts
behaviour.md            how agents work here: always told
concepts/<name>.md      what a word of the team means: looked up
recipes/<name>.md       how something is done, step by step: looked up
tools/<name>.toml       a tool every agent has
parts/<part>/…          the same four, for the repositories that pick it
```

- **Behaviour** is short and always given: at the start of a session,
  with Kito's own rules. It is the only part an agent does not ask for.
- **Concepts** and **recipes** are named, with one line saying what each
  is about (the first line of the file). An agent is told the names and
  the lines, and asks for the text when it needs it: a kitchen can be
  large without being in every prompt.
- **A tool** is a command and what it takes (`command`, `args`, `about`,
  `input`, `timeout_secs`), run in the lane's working copy on the
  machine of whoever works. It is offered to agents next to Kito's own.
- **A part** (`parts/rust`, `parts/web`) is optional. A repository on
  the server picks parts: `kito-server kitchen parts <repository>
  rust web`. What it gets is the kitchen's base and its parts, a part
  winning over the base where both have the same name.

### 2. Plugins in a connected directory, and a server's plugin asked

- **A directory connected to a server runs plugins**: those the *server*
  enabled for clients, if the person has them installed. The server says
  which (`kito-server plugin enable kitchen --for-clients`); a person
  enables nothing in a connected directory, and a plugin missing on
  their machine is said with how to get it. Gates stay the server's.
- **`plugin_call`**, a new method of the daemon's API (version 2): a
  plugin asks *its own counterpart on the server*, `{ "ask": name,
  "params": … }`. The server runs `kito-<plugin> answer <name>` with the
  params on its input, the server's directory, and who asks (their name,
  the repository, their role) in its environment, and passes the answer
  back. A plugin reaches only the plugin of the same name; the server
  enabled both; the answer is data. Permission: `read`.
- On a machine with no server (a repository of its own), `plugin_call`
  is refused: there is nobody to ask.

### 3. The plugin `kitchen`

One program, `kito-kitchen` (`KitoOrg/kito-kitchen`), on both sides:

- **On the server**: `kito-server kitchen set | parts | show | check`
  (`server_admin`), and `answer`: what a repository's kitchen is now
  (behaviour, the names and lines of concepts and recipes, tools), one
  concept, one recipe, a tool's files. Read from the kitchen
  repository's main lane, as integrated: what is in a lane of the
  kitchen is not the kitchen yet.
- **For agents**, through `kito-mcp` as tools of the plugin: `kitchen`
  (the behaviour and what there is), `concept`, `recipe`, and each of
  the kitchen's tools by its name. At the start of a session the
  behaviour and the lists are given with Kito's rules.
- **For people**: `kito kitchen` shows the kitchen of this repository;
  `kito kitchen concept | recipe <name>` prints one.
- **Assistants** need nothing more: the kitchen comes through Kito's own
  server (`kito-mcp`) and hooks, which every assistant plugin already
  sets up. Nothing of the kitchen is copied into `AGENTS.md` or
  `CLAUDE.md`, so nothing goes stale there.

### 4. What is trusted

- A kitchen's text is what a team wrote for its agents: it is given as
  the team's instructions, said to be the kitchen's, and never as
  Kito's own rules. Kito's three rules are not the kitchen's to change.
- A kitchen's tool runs a command on the machine of whoever works, as
  a repository's own scripts do. It is run confined like a check where
  checks are confined (Linux), in the lane's working copy, and only
  what the kitchen names. Who may change the kitchen may run commands
  on the team's machines: that role is given accordingly, and changes
  to `tools/` are reviewed like code.

## Consequences

- The daemon's API becomes version 2 (one method added; version 1
  plugins go on working).
- `kito-server` learns one more thing about a repository (its kitchen
  parts) and one about itself (which repository is the kitchen).
- A person needs `kito-kitchen` installed once; until then the session
  says the team has a kitchen and how to get the plugin.
- An agent with no connection to the server has the kitchen it last
  got, kept in the directory, and is told how old it is.
- Not in this ADR: a kitchen without a server (a repository pointing at
  one), kitchens shared between servers, MCP servers as kitchen tools
  (a command only, for now), a kitchen page in the web client.

## Order of work

One phase (21), released as 0.22.0, in three parts:

1. plugins in a connected directory, `--for-clients`, `plugin_call` and
   `answer` (with `kito-hello`), API version 2;
2. `kito-kitchen`: the kitchen repository, parts, behaviour, concepts
   and recipes, for agents and for people;
3. the kitchen's tools, the session start, the copy kept for when the
   server is away; documents and the demo.
