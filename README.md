# Kito

A semantic version control system, written in Rust, for swarms of AI agents.

The unit of history is the semantic operation on a symbol, captured
continuously: dozens of agents work in parallel on the same repository and
run into almost nothing but real conflicts. Every checkpoint on the main lane
is exported as a git commit.

The project is built in phases; each phase ends with green tests and a
runnable demo.

| Phase | Content | Status |
| --- | --- | --- |
| 0 | Foundations: `kito-core`, `kito-store` | done |
| 1 | Continuous textual capture: `kitod`, CLI, git export/import | done |
| 2 | Level 1 semantics: tree-sitter, stable IDs, `kito why` | done |
| 3 | Lanes and integration, MCP server | done |
| 4 | Level 2 semantics: LSP, `SemanticRef` conflicts | done |
| 5 | Coverage and performance | done |
| 6 | The intent board: declared scope, overlaps between lanes | done |
| 7 | One server for several repositories, clients on other machines | done |
| 8 | Lanes from any machine: one writes, others follow, a journal of saves | done |
| 9 | Protected main and team rules: roles, approval, checks and git push on the server | done |
| 10 | The why graph: the reasons of every function, file and line, to ask before changing them | done |
| 11 | The rules as behaviour: hooks that refuse an edit outside a lane or its scope, and the reasons told before code changes | done |
| 12 | Encrypted connections and a sandbox per lane: a server known by its certificate; checks and lane commands that write only in their own copy | done |
| 13 | Integration without waiting in line: lanes that do not depend on each other are checked in one run | done |
| 14 | Git both ways: commits made with plain git are brought in through a lane, merged and pushed back | done |
| 15 | Lanes on lanes: a lane opened on work that has not landed, landing on it or further down ([ADR 0001](docs/adr/0001-lanes-on-lanes.md)) | done |
| 16 | Permanent lanes: a `staging` next to `main`, with its own checkpoints, git branch and checks | done |
| 17 | Review of a lane ([ADR 0004](docs/adr/0004-review-of-a-lane.md)): its diff, comments its agent is told, verdicts, approvals before integration; data and questions, no interface | done |
| 18 | The why graph, better: intents that say nothing refused, notes on code ([ADR 0003](docs/adr/0003-notes-on-code.md)), lines followed across file renames | done |
| 19 | Plugins ([ADR 0005](docs/adr/0005-plugins.md)): programs that add commands, gates of integration, event handlers, tools for agents and assistants, within the permissions a repository gives them; the daemon's API, numbered | done |
| — | Windows ([ADR 0002](docs/adr/0002-windows.md)): the tests pass there, in CI; no sandbox, no partial-save detection | done |

## Crates

| Crate | Responsibility |
| --- | --- |
| `kito-core` | Data model types, canonical CBOR encoding, IDs and hashes |
| `kito-store` | Blob packs, segmented oplog, rebuildable indexes, crash recovery |
| `kito-parse` | tree-sitter grammars and symbol queries for the ten languages, normalized hashes |
| `kito-diff` | Textual diff, file ops, and the AST differ that keeps symbol IDs stable |
| `kito-merge` | The semantic rebase: three states composed symbol by symbol, conflicts |
| `kito-semantic` | Who uses a symbol: from the syntax, or from a language server |
| `kito-lane` | Working copies of the lanes, cloned copy-on-write where the file system allows |
| `kito-integrate` | Integration queue, verifier, the result an agent reads |
| `kito-git` | Export of checkpoints as git commits, import from a git repository |
| `kitod` | Daemon: watcher, capture on every lane, integration, JSON-RPC socket; in a connected directory, the lanes kept in step with a server |
| `kito-cli` | The `kito` binary |
| `kito-mcp` | The MCP server agents talk to |
| `kito-server` | One server for several repositories: who a client is, what they may use |

## Trying it out

```bash
cargo build --release -p kitod -p kito-cli -p kito-mcp
```

```bash
export PATH="$PWD/target/release:$PATH"
```

In a new directory (in an existing git repository use `kito import`):

```bash
kito init
```

It also sets up the coding assistants you use (see
[Coding assistants](#coding-assistants)): the ones you said once with
`kito assistant default`, or, asked at the terminal, the ones found on
this machine; with nobody to ask, Claude Code.

From here on every save is recorded, with no commits. To see what happened
and mark a point, which also becomes a git commit:

```bash
kito log
```

```bash
kito checkpoint -m "first version"
```

The log has a line for each file worked on, however many times it was saved,
with what the saves meant for its symbols; `kito log -v` gives every save a
line of its own:

```text
  b2bc7079  11-14 21:53  francesco  src/version.ts  +1 −1            VERSION changed
◆ ckpt#2  billing  (git 78ff4e73aa)
  928f6f58  11-14 21:53  claude     src/billing.ts  new              invoice created  [integrate lane/1: billing]
  071ff037  11-14 21:53  francesco  src/web.ts      +1 −1            profile changed
  9239b953  11-14 21:53  francesco  src/auth.ts     +10 −0, 7 saves  getUser changed; getAdmin created
◆ ckpt#1  start  (git f908e3a85f)
```

In Python, JavaScript, TypeScript, Java, C#, C, C++, Go, PHP and Rust files
Kito also follows the symbols: a function keeps its identity when it is
renamed, edited or moved to another file. Its whole story, asked by the name
and the place it has today:

```bash
kito why --history src/users.ts:fetchUser
```

To go back to a checkpoint or to an earlier op (recorded as new ops, so it
can be undone), and to set who signs the changes:

```bash
kito rollback ckpt#1
```

```bash
kito identity --name "Ada" --email ada@example.com
```

## Agents and lanes

Every agent works in a lane: a working copy of its own, where its saves are
recorded like yours. A lane is opened by saying why, and what it will
change; opening one prints where it is:

```bash
kito lane open alice --intent "taxes on every total" --scope src/shop.ts:total
```

When the agent is done, its work is composed with what the main lane has
become meanwhile, symbol by symbol: two agents that changed different
functions of the same file, or added functions at the same place, do not
conflict.

```bash
kito integrate lane/1
```

Commands that must succeed before anything reaches the main lane (a type
checker, the tests) go in `.kito/config.toml`:

```toml
[verify]
commands = ["npx tsc --noEmit", "npm test"]

[lanes]
share = ["node_modules"]
```

A real conflict — the same lines of the same function, a symbol removed on
one side and changed on the other — stops that lane only, and is decided by
taking one side or writing the text:

```bash
kito conflicts
```

```bash
kito resolve conflict#1 --lane
```

A rename made in one lane reaches the uses another lane added meanwhile
under the old name. With a language server installed that happens by
itself: `typescript-language-server`, `gopls`, `pyright`, `rust-analyzer`,
`intelephense` and `clangd` are looked for by default, others go in
`[semantic.servers]` of the configuration. An index made by a SCIP indexer
over the whole repository does the same for the files that have not changed
since (`kito scip index.scip`). Without either, Kito reads the syntax and
asks before it touches a use it is not sure of:

```bash
kito rename src/auth.ts:getUser fetchUser --lane lane/1
```

```bash
kito resolve conflict#1 --rewrite
```

### Lanes on lanes

An agent can build on another agent's work before it lands: a lane opened
`--on` another starts from it as it is, and lands on it.

```bash
kito lane open bob --intent "show the team on the page" --scope src/page.py --on lane/1
```

Landing on a lane makes no checkpoint and runs no checks: the main lane does
not move, and the work is checked when it gets there, with the lane it
landed on. It is not queued behind lanes landing on the main lane. The lane
landed on takes on the scope of what landed on it, since it brings it
further, and `kito why` follows each line back to the lane that wrote it.

A lane can also land further down than the lane it starts on, on main for
instance. It then waits until the work it was built on is there, and brings
only its own:

```bash
kito lane open carol --intent "the team is tested" --scope tests/ --on lane/1 --target main
```

A lane with lanes on it closes once the work they were built on has
landed; they are then moved to where it went. Lanes stack as deep as
needed. The design is [ADR 0001](docs/adr/0001-lanes-on-lanes.md).

### Permanent lanes

A `staging` next to `main`, that work lands on before it reaches main, and
that CI deploys from:

```bash
kito lane create staging
```

```bash
kito lane open alice --intent "users have a team" --scope src/api.py --on staging
```

Landing on a permanent lane makes a checkpoint of it, runs its checks, and
exports the checkpoint to a git branch of its own (`staging`), pushed with
main's. What lands on main directly comes into staging with `kito lane
catch-up staging`. When staging is good, it lands on main as a git merge
and stays open, brought up to main:

```bash
kito integrate staging
```

Its checks, its branch, and who may land on it go in the configuration:

```toml
[lanes.staging]
verify = ["npm test"]
branch = "staging"
integrate = ["ci"]
```

Agents do all this through the MCP server, `kito-mcp`, which offers
`kito.open_lane`, `kito.sync`, `kito.status`, `kito.log`, `kito.set_intent`,
`kito.rename_symbol`, `kito.request_integration`, `kito.integration_result`,
`kito.why`, `kito.resolve_conflict` and `kito.close_lane`. In the
configuration of an MCP client:

```json
{
  "mcpServers": {
    "kito": { "command": "kito-mcp", "args": ["--repo", "/path/to/the/repository"] }
  }
}
```

Started inside the repository it finds it by itself, and `--repo` can be
left out. If the client does not accept a dot in tool names, add
`"--tool-prefix", "kito_"`.

### Coding assistants

Every assistant is set up the same way, in a repository:

```bash
kito assistant add claude        # Claude Code, by Kito itself
kito assistant add codex zcode   # by their plugins: installed and enabled here when needed
kito assistant remove codex      # what `add` wrote in the project comes out
kito assistant list              # what Kito knows, what this machine has, how each is set up
```

For an assistant a plugin knows, `add` uses the plugin enabled here; or
the one installed that knows it, which it enables here; or the plugin of
that name in the registry, which it installs and enables. Each step is
said. ZCode runs no hooks of a project, so `add zcode` also writes Kito's
hooks in your own ZCode settings (`kito zcode hooks remove` takes them
out).

Kito finds Claude Code, Codex and ZCode on a machine by their program on
the `PATH` or their settings directory (`~/.claude`, `~/.codex`,
`~/.zcode`). Which ones every new project gets is said once:

```bash
kito assistant default claude codex   # kito init, import and connect set these up
kito assistant default --none         # none
kito assistant default                # says it
```

With nothing said, `kito init` asks at the terminal, one question for each
assistant found; with nobody at the terminal (a script, an agent) it sets
up Claude Code, as it always did, and names the others found.
`--assistant <name>` (again for more) says it for one project,
`--no-assistants` sets none up, `--no-claude` leaves Claude Code out. The
install script asks the same question once, and writes the answer with
`kito assistant default`.

### Instructions

What the assistants are told is kept by Kito, not in the project: it is
shared with everyone who works in the repository, through its server
when it has one, and Kito writes it into `CLAUDE.md` and `AGENTS.md` at
every session. Those files, and the assistants' settings (`.mcp.json`,
`.claude/`, `.codex/`, `.zcode/`, …), stay out of the history and of git
(`[agents] files_in_history = true` records them again).

One standard, with a kitchen first and the repository's own over it:

```text
<kitchen>/instructions/all/style.md     every assistant, every repository of the kitchen
<kitchen>/instructions/codex/plan.md    Codex only
kito instructions set style             this repository's all/style.md: replaces the kitchen's
```

```bash
kito instructions                       # what is told, and where each part comes from
kito instructions show --for codex      # AGENTS.md as Codex gets it
kito instructions set all/tests --file tests.md
kito instructions remove all/tests
kito instructions kitchen ../kitchen    # a directory with instructions/
kito-server repo set shop --kitchen kitchen   # on a server: another of its repositories
```

Kito's rules always come first. A `CLAUDE.md` or `AGENTS.md` that was
there before becomes an instruction the first time Kito writes it; lines
written outside Kito's marks afterwards stay on that machine only.

### Claude Code

`kito assistant add claude` (or `kito claude`, the same) writes:

| File | What goes in it |
| --- | --- |
| `.gitignore` | `.kito/`, so that git does not see Kito's own directory, and the files assistants read |
| `.mcp.json` | the `kito` server: `{ "command": "kito-mcp" }` |
| `.claude/settings.json` | `mcp__kito__*` among the allowed tools: Claude calls them without asking each time; and three hooks (`kito hook`), which hold Claude to the rules whatever it decided to do: see [The rules as behaviour](#the-rules-as-behaviour) |
| `CLAUDE.md` | the rules: one lane per task, edits only in the lane's working copy, `rename_symbol` for renames, integrate at the end; then the [instructions](#instructions) |

Each file is touched only for what is missing in it: what is already there
stays as it is written, and a second run changes nothing. A file that is not
what was expected (JSON that does not parse, say) is left alone and named.
They are not files of the project: Kito keeps them out of its history and
of git, and whoever clones the project gets them with `kito assistant add
claude`, the instructions with them. To leave a
project as it is, `kito init --no-assistants` (or `KITO_NO_CLAUDE=1` in
the environment); `kito assistant remove claude` takes them out.

Then start Claude Code in the project directory. The first time it asks to
trust the folder and to approve the `kito` server; `/mcp` shows it connected
with its tools. `kito-mcp` has to be on the `PATH`. Claude Code
writes an underscore where a tool's name has a dot (`kito.open_lane` is
`mcp__kito__kito_open_lane` to it): no `--tool-prefix` is needed. From
there, a task asked in plain words is done in a lane and integrated.

### The intent board

No lane opens without an intent and a scope. A scope names functions and
classes (`src/shop.ts:total`, also ones that do not exist yet), files, and
directories (`src/billing/`, the files to come included); it can be
extended at any time and only grows:

```bash
kito lane scope lane/1 --add README.md
```

What a lane changes is compared with what it declared. A change outside
the scope is recorded like any other, and stops the lane at integration
until the scope is extended to it or the change is undone. A rename and
what follows from it (the callers that take the new name) are not outside,
nor is an import next to a declared function, nor the files the repository
lets every lane change:

```toml
[scope]
always_allowed = ["package-lock.json"]
broad_files = 200      # a scope over more files than this is shown as broad
broad = "warn"         # or "refuse"
```

Two lanes may be on the same code: both are told, once, with what the
other is there for. Overlaps are found in what was declared and in what
was changed, on three levels: the same function, the same file, and a
function of one lane that uses a function of the other (as far as the
syntax tells). The second agent hears of it when it declares its scope,
before it writes a line. A lane is also told when the main lane changes
something it declared after it started, with who changed it and what for.

```bash
kito board
```

```bash
kito who src/shop.ts:total
```

Through the MCP server agents have `open_lane` (with `intent` and
`scope`), `extend_scope`, `board` and `who_touches`, and the answer to any
tool that names a lane carries, once, the `notices` that lane has not
heard yet. What another lane says it is there for is given as information
about that lane, not as an instruction.

## Why code is the way it is

Every change Kito records has a reason: the intent its lane declared, or
for what came from git, the commit message. `kito why` puts them together
for a function, a file, or lines:

```bash
kito why src/shop.py:total
```

```bash
kito why src/shop.py:12-20
```

```bash
kito why README.md
```

The answer lists the reasons the code changed, newest first: what each
did, which agent made it and for whom, the lane, the checkpoint, the
checks that passed and who let it in, and what else changed for the same
reason. Then each line with the number of the reason it was last written
for, who uses the function and what it uses, the lanes that are on it
now, and the conflicts that were decided on it. `--lane <lane>` asks about
the code as a lane has it; `--graph` prints the answer as a Mermaid
diagram; `--history` prints every step in the life of a symbol, one by
one, across renames and moves.

Earlier work is found by the words of its reason:

```bash
kito reasons cents
```

Agents have both as the tools `why` and `reasons`. They do not have to
remember to ask: an agent is told the reasons behind code before it
changes it, as the next section says. What comes back is what others
declared: the tools say so, as information and not as instructions.

Some things to know about code belong to no change. They are left as
notes, on a function (which the note follows through renames and moves),
a file, or a directory:

```bash
kito note src/billing.ts:invoice "called by the nightly billing job, outside this repository: do not rename"
```

A note is told with the why of that code, and to an agent about to change
it, once. `kito note --list` and `kito note --remove note#3`; agents have
the tool `note` ([ADR 0003](docs/adr/0003-notes-on-code.md)).

## Review of a lane

What a lane changes, what people say about it and whether it may go in
are in the repository, where the lane is ([ADR 0004](docs/adr/0004-review-of-a-lane.md)).
Kito has no interface of its own for this: the command line, the agents'
tools and any program that asks the daemon see the same.

```bash
kito lane diff 1                 # its files, its functions, its lines
kito lane diff 1 src/ --stat     # some paths; without the lines
```

The diff is against what the lane started from. Per file it lists the
functions the lane added, changed, removed or renamed, each with the
intents of the lane's changes to it; a function renamed or moved is one
change, with the name it had.

```bash
kito lane comment 1 "Half a cent is lost: round, do not floor." --on src/shop.ts:4
kito lane comment 1 "Rounded now." --reply comment#1
kito lane comments 1
kito lane resolve-comment comment#1
```

A comment is on the lane, on a function, a file or lines. On lines it
keeps what they said: it stays on them wherever they move, and is marked
once the lane no longer has them. **The agent in the lane is told**: its
first edit after a comment is held back once, with the comments it has
not read. A comment is how a person tells an agent in a lane what to
change.

```bash
kito lane request-changes 1 "See the comment on line 4."
kito lane approve 1
kito lane withdraw 1
kito lane review 1               # where it stands, and what is missing
```

A request for changes stops the lane until it is taken back or replaced.
An approval is of the lane as it was: once the lane changes it is stale.
The owner's own approval does not count. **Approving brings nothing in**:
`kito integrate` does, as before, for whoever may.

A repository may ask for approvals before a lane is integrated:

```toml
[review]
required = 1      # approvals, from people other than the lane's owner
agents = false    # whether an agent's verdict counts, as its person's
keep = false      # whether an approval survives a change to the lane
```

On a server it is the repository's rule, like its checks: `kito-server
repo set shop --approvals 1`. A lane that lacks its review is merged,
checked, and then waits, saying what is missing (`awaiting_approval`).
Who reviewed is on the checkpoint, in the why of that code, and in the
exported commit (`Reviewed-by:`). Agents have the tools `lane_diff`,
`comment`, `comments`, `resolve_comment` and `review`.

## Plugins

What does not belong in Kito is added by plugins: programs of their own,
which ask the daemon what `kito` asks ([ADR 0005](docs/adr/0005-plugins.md)).
A web client is one; so is what sets up a coding assistant other than
Claude Code. Neither is part of Kito.

```bash
kito plugin add --path ./kito-web   # installed for you
kito plugin enable web              # this repository lets it run, with what it asks for
kito web                            # its command, now one of Kito's here
kito plugin list
```

Nothing of a plugin runs in a repository that did not enable it, and the
daemon answers it only what the repository gave it: `read`, `write`,
`integrate`, `admin` (`kito plugin enable web --only read` gives less
than it asks for).

A plugin is `kito-<name>` and a `kito-plugin.toml` next to it, which says
what it asks for and what it adds:

```toml
name = "web"
version = "0.1.0"
api = 1                              # the daemon API it was written for
permissions = ["read", "write"]

[[command]]                          # `kito web` runs `kito-web`
name = "web"
about = "Serve the web client for this repository"

[[gate]]                             # asked before a lane is integrated
name = "licence"
about = "Every new file has a licence header"

[[event]]                            # run for these, one at a time
kinds = ["integrated", "commented"]

[[tool]]                             # offered to agents as kito.web.open
name = "open"
description = "The address of a lane in the web client"

[assistant]                          # `kito assistant add codex`
name = "codex"
```

- **A command** is run in the repository with `KITO_REPO`, `KITO_API`,
  `KITO_SOCKET` (where the daemon listens) and `KITO_PLUGIN_KEY` in its
  environment: the key is what it says to the daemon, which then knows
  it as that plugin.
- **A gate** (`kito-web gate licence`) is asked after the checks, in the
  same confined copy of the merged result, with the lanes in it and
  their diffs as JSON on its input. It prints `{"result": "pass"}`,
  `"fail"` or `"wait"` with a `"message"`: a failure is a check that
  failed; a wait leaves the lane waiting with that reason, and the gate
  is asked again the next time.
- **An event** (`kito-web event`, the event as JSON on its input) is told
  after the fact, and never holds anything up. What a plugin last failed
  at is in `kito status`.
- **A tool** (`kito-web tool open`, the arguments as JSON on its input)
  is offered to agents after Kito's own, and says whose it is.
- **An assistant** is set up by `kito assistant add <name>`, which
  enables the plugin here if needed and runs its `assistant setup` in the
  repository (installing it from the registry first when it is not
  installed).

A server has plugins too ([ADR 0006](docs/adr/0006-server-plugins.md)):
`kito-server plugin add --path <dir>`, then `plugin enable <name>`. On a
server a plugin adds commands to `kito-server` (`[[server_command]]`)
and is told what happens there (`[[server_event]]`: a repository added,
a person added or removed, a role changed).

`kito-server serve --http 0.0.0.0:7443` answers HTTP too, with the
server's certificate: `/-/me`, `/-/session` and `/-/api/<repository>`
are the server's own, and every other path goes to the plugin that
serves pages there (`[pages]`), which is told who is signed in and may
ask of a repository only what that person may do.

People sign in through a plugin as well (`[sign_in]`): the plugin says
who somebody is, the server says what they may do.

```console
$ kito-server user add bob --identity github:bob --role write
bob added: whoever signs in as github:bob is bob. No token.
$ kito connect kito.example.com/shop
To sign in with github, open https://github.com/login/device
and enter the code ABCD-1234
(opened in your browser)
Waiting until you have...
shop is connected to `shop` on kito.example.com:7421 as bob.
```

The session lasts thirty days (`[sign_in] days` in `server.toml`);
`kito sign-in` gets another. A browser goes to `/-/sign-in/github` and
comes back with a session cookie. Tokens stay for scripts and agents.
`scripts/demo-phase20.sh` shows a server and its plugin at work.

A server says which of its plugins run in the directories connected to
it too (`kito-server plugin enable <name> --for-clients`), for whoever
has them installed; there a plugin asks its counterpart on the server
(`plugin_call`), and may have its say when a session starts
(`[session]`). That is what a team's **kitchen** is made of
([ADR 0007](docs/adr/0007-kitchens.md), the plugin `kito-kitchen`): how
the team's agents behave, what its words mean, how things are done and
the tools everybody has, written once in a repository on the server and
given to every agent, whatever assistant it is.

```console
$ kito-server plugin enable kitchen --for-clients
$ kito-server kitchen set kitchen
$ kito-server kitchen parts shop rust
$ kito kitchen                          # in a directory connected to it
The kitchen of Shopworks for `shop`, with rust
…
```

Plugins are installed by name from a **registry**
([ADR 0008](docs/adr/0008-plugin-registry.md)): files that list each
plugin's versions with their checksums, at `https://kito.sh/registry` or
wherever `KITO_REGISTRY` says (a team's own; a directory, with
`file://`).

```console
$ kito plugin search
$ kito plugin add kitchen             # checked against the registry's SHA-256
$ kito plugin outdated
$ kito plugin update
$ kito plugin disable kitchen         # it takes out what it wrote here
$ kito plugin remove kitchen
```

On a server, the people its machine named administrators (`kito-server
user admin <name>`) do the same from a browser (`/-/admin/plugins`, the
page is in `kito-web`): by name, from the server's registry, and nothing
else. The registry at kito.sh lists nothing yet.

A registry is **signed** ([ADR 0009](docs/adr/0009-signed-registry.md)):
`kito registry key <file>` makes its key, `kito registry sign <directory>
--key <file>` signs it, and whoever sets `KITO_REGISTRY_KEY` takes only
what that key signed. A signed index is numbered and does not go back.

What a program may ask the daemon, and under which permission, is in
[docs/api.md](docs/api.md): the daemon's API, version 2.
`crates/kito-cli/examples/kito-hello.rs` is the smallest plugin there is,
and `scripts/demo-phase19.sh` shows it at work.

## The rules as behaviour

Three rules hold for every agent, and none of them depends on the agent
choosing to follow it:

1. **Work in a lane.** An edit in the main directory is not made.
2. **Change what the lane declared.** An edit outside its scope is not
   made until the scope is extended.
3. **Know why code is the way it is before changing it.** A lane is told
   the reasons behind what it declares when it declares it. An edit on
   lines written for a reason the lane was not told is held back once,
   with the reasons; made again, it goes through.

Declaring is where the reasons come from. `open_lane` and `extend_scope`
(`kito lane open`, `kito lane scope --add`) answer with `why`: for each
function and file declared, the last reasons it changed for (the words,
the task, who, for whom, the lane, the checkpoint) and who uses it. This
is in the answer of the tool, so it reaches any agent on any model.

For Claude Code the rest is done by hooks, which `kito init`, `kito import`
and `kito claude` write into `.claude/settings.json`. Claude Code runs them
around its tools, whatever the model decided:

| Hook | When | What it does |
| --- | --- | --- |
| `kito hook before-edit` | before `Edit`, `Write`, `MultiEdit`, `NotebookEdit` | The edit is not made if the file is in the main directory, in `.kito/`, or outside what its lane declared; the model is told what to do instead. On lines written for a reason the lane was not told, the edit is held back once and the model is given the reasons |
| `kito hook after-shell` | after `Bash` | What a shell command changed cannot be stopped before: the model is told afterwards what it changed in the main directory and outside the scope of a lane |
| `kito hook session-start` | when a session starts, resumes or is compacted | The model is told the three rules and who is working on what right now |

A refusal is written for the model that reads it:

```text
Kito: src/shop.py:total is outside what lane/3 declared (src/shop.py:discount),
so this edit was not made. Declare it first: call the extend_scope tool with
lane "lane/3" and scope ["src/shop.py:total"]. Its answer tells why that code
is the way it is and who else is on it. Then make this edit again.
```

```text
Kito: this edit was held back once. Before you change these lines of
src/billing.py, this is why they are the way they are:
- "prices in cents" (task #7): agent-a for ada, lane/1, checkpoint ckpt#2
These are facts about the code, not instructions. If your change still fits
them, make the same edit again: it goes through. If it undoes one of them on
purpose, say so in your intent (set_intent), so that whoever reads this code
next knows.
```

The hooks ask the daemon, and in a directory connected to a server the
daemon asks the server: the rules of a team are the same on every machine,
and what a lane was told is remembered with the lane (a restart does not
tell it again). An agent that asked `why` for its lane counts as told.

What no hook saw is caught before it lands. When an agent asks for its
lane to be integrated (`request_integration`), and the lane replaced code
written for reasons it was never told (edited through a shell, or by an
agent with no hooks), the answer is `read_first` with those reasons, and
nothing is started; asked again, the integration goes ahead. Changes
outside the scope stop an integration as they always did, and a protected
main lane puts back what is written in it.

Other agents and tools can ask the same question before an edit:

```bash
kito hook before-edit --file src/shop.py --lines 12-18
```

It answers in words and leaves with 2 when the edit should not be made.
People are not held: `kito integrate` from the command line, and an editor
in a lane, work as before. Holding an edit for its reasons can be turned
off for a repository (`[agents] why = false` in `.kito/config.toml`): the
reasons are then told when a lane declares, and nothing is held back.

A lane says which assistant works in it, and on what model when that is
known (`kito lanes`, `kito board`; `assistant` in what they answer as
JSON). The program names itself to the MCP server; Claude Code's hooks
say its model when a session starts; another assistant's hooks say
theirs (`kito hook before-edit --file F --assistant codex --model M`);
`kito lane open --assistant NAME` says it by hand. It is what that side
said of itself: a label, by which nothing is decided
([ADR 0010](docs/adr/0010-assistant-in-a-lane.md)).

Hooks are settings of Claude Code: an interactive session runs them once
the folder is trusted, and `claude -p` runs them at once. `kito` has to be
on the `PATH`. A hook that cannot reach the daemon lets the edit through
and says so on its standard error; the rules are still checked when the
lane is integrated.

## A server for several repositories

`kito-server` keeps several repositories behind one door. Each is an
ordinary Kito repository with its own daemon; the server knows who a client
is and which repositories that person may use, and passes the requests on.

```bash
kito-server init /srv/kito
```

```bash
kito-server --dir /srv/kito repo add shop --from /path/to/a/git/repository
```

```bash
kito-server --dir /srv/kito user add ada --email ada@example.com
```

`user add` prints the person's token once; the server keeps only what it
hashes to. `--repo <name>` limits a person to some repositories, and
`--role` says what they may do there (see "The rules of a team").

```bash
kito-server --dir /srv/kito serve
```

A client connects a directory of its own to a repository. The files of the
main lane come into it and are kept as the server has them, and from then
on `kito` in that directory works as it does in a repository:

```bash
kito connect kito.example.com/shop      # or kito clone; kito://host:port/shop
```

With no token, Kito asks the server how to sign in, lets you pick at the
terminal (a way in the browser, or pasting the token `user add` printed)
and opens the page. A server whose certificate is its own is shown by its
fingerprint, once, for you to say it is yours. Said on the command line,
for a script:

```bash
kito connect host:7421/shop --token kito_... --fingerprint sha256:...
```

```bash
kito watch
```

`kito watch` prints what happens as it happens: lanes opened and closed,
scopes extended, saves, integrations and how they end. A lane opened
through the server belongs to the person the client was authenticated as,
whatever the client says, and the board shows whose agent each lane is.

What travels between a client and the server is encrypted (TLS). The
server makes a key and a certificate for itself the first time it needs
them (`tls/` in its directory), and clients know it by the certificate's
fingerprint, as they know an SSH host by its key: `user add` prints the
whole `kito connect` line with it, and this prints it again:

```bash
kito-server --dir /srv/kito fingerprint
```

A client that is given a fingerprint keeps it (`.kito/remote.toml`): a
server that presents another certificate is refused before the token is
sent. A server on the same machine may be connected without one, and is
held to the certificate it showed the first time. For a server behind a
certificate from a public authority, put the certificate and its key in
`tls/cert.pem` and `tls/key.pem`, and connect with `--authority`: the
server is then known by its name. To listen on more than this machine:

```bash
kito-server --dir /srv/kito serve --listen 0.0.0.0:7421
```

`serve --plain` (and `kito connect --plain`) is the server as it was before
0.12: nothing encrypted, for clients on the same machine only.

The other way in is SSH, where the person is who SSH says they are:

```bash
kito connect ssh://ada@host/srv/kito shop
```

For that, give each key a forced command in `authorized_keys` that names
its person: `command="kito-server --dir /srv/kito stdio --as ada"`.

What a client may ask is what lanes are for: seeing them, opening and
closing them, declaring scope, integrating, deciding conflicts. Stopping a
daemon, checkpoints of the main lane and rollbacks stay with whoever runs
the server.

### Lanes from any machine

A lane is kept by the server; its working copy is wherever it is worked
on. Opened from a connected directory, it is edited on that machine:

```bash
kito lane open agent-a --intent "prices in cents" --scope src/shop.py
```

The answer names a working copy under the directory. Every save there goes
to a journal on disk and from it to the server, where it is recorded in the
lane as the agent's; what the server changes in the lane (an integration, a
rename) comes back. `kito sync <lane>` returns when everything saved so far
is on the server, and so do an integration request and a rename; if
something did not get there they say why instead of going on without it.

Anybody who may use the repository can follow a lane from their own
machine:

```bash
kito lane attach 1
```

Its files are then on that machine too and change as the lane's do, a few
hundredths of a second after each save. The lane's owner can carry on from
another machine with `kito lane attach 1 --write`: one machine writes in a
lane at a time, and the other follows from then on.

```bash
kito daemon status
```

shows the lanes kept on this machine, which it writes and which it
follows, and anything waiting: saves not yet on the server, files that
were changed here and on the server in ways that do not merge (`kito lane
take <lane> <file> --mine` or `--theirs` settles one), files changed in a
copy that only follows. `kito lane detach <lane>` removes this machine's
copy; the lane stays.

A daemon killed in the middle of the work loses nothing: the journal and
the files are where it left them, and the next command starts it again.

### The rules of a team

A repository added with its history is protected: its main lane changes
only when a lane is integrated. A file edited in the main directory on
the server is put back, the edit is kept under `.kito/refused/`, and
`kito watch` shows it. The checks every lane must pass, and the git remote
that gets every integration, are set on the server:

```bash
kito-server --dir /srv/kito repo set shop --check "npm test" --push origin
```

```bash
kito-server --dir /srv/kito repo list
```

People have roles. `read` sees and follows; `write` opens lanes and works
in them; `integrate` brings lanes into the main lane:

```bash
kito-server --dir /srv/kito user add bob --role write
```

```bash
kito-server --dir /srv/kito user role bob integrate --repo shop
```

When somebody who may write asks for a lane to be integrated, it is
merged and checked as for anybody, so conflicts and failing checks come
back at once; if all is well it waits (`awaiting approval` on the board)
until somebody who may integrate runs `kito integrate <lane>`, which
merges and checks again. A lane is changed only by its owner or by
somebody who may integrate.

The same protection is there for a repository with no server:

```bash
kito protect
```

```bash
kito protect --off
```

What the protection does not do is listed in
[`docs/phases.md`](docs/phases.md): it undoes an edit within milliseconds
rather than preventing it, and whoever runs the server can lift it.

### Git both ways

Not everybody on a team works through Kito. Somebody commits with plain
git and pushes to the remote Kito pushes to. That used to stop the
pushes, which are never forced, until the two histories were put together
by hand. Now the commits the remote has and the main lane does not are
brought in like any other work, through a lane, before every push:

- the lane starts from the checkpoint the commits were made on, and each
  commit is replayed in it under its own author, with its message as the
  reason and its hash as the task;
- the lane is integrated like every lane: merged function by function
  with what the main lane did meanwhile, and checked;
- the checkpoint of that integration goes to git as a merge with the
  remote's last commit, so the remote takes the push and whoever works
  with git pulls it as usual.

```text
*   2109afb git: merge of commits made with git: say which currency
|\
| * b9b01cb Grace: say which currency
| * 10bfe60 Grace: whole cents off
* | 46c34fd agent-a: prices in cents
|/
* b7e62d2 ada: a shop that adds up prices
```

`kito why` then answers for that code with the commit's message, under
its author's name. Commits that conflict with what a lane did wait in
their lane: `kito status` says so, nothing is pushed over them, and after
`kito conflicts` and `kito resolve` this brings them in:

```bash
kito pull
```

It is also the way to bring commits in without waiting for the next
integration. It needs a remote (`[git] push`, or `kito-server repo set
<name> --push <remote>`); on a server it is run by whoever runs the
server, in the repository's directory.

### Lanes checked together

With checks that take time, lanes used to wait in line: ten agents that
finish together waited for ten runs. Now the lanes that ask while a run is
going are taken together when their turn comes: their merged results are
put in one copy, the checks run once, and each lane is then integrated
under a checkpoint of its own, in the order they asked.

```bash
kito integrate lane/1 lane/2 lane/3
```

```text
lane/1: integrated as ckpt#7 (git 8d020c87a9), verified in 2.0 s together with 2 other lanes
```

Only lanes that do not depend on each other are checked together: their
merges write different files, and none of them renames or moves anything.
Two lanes on the same file are still merged function by function, and
checked one after the other. If a run fails, the lanes are split in two
and each half is tried again, until the lane that fails is found by
itself: it is refused, the others go in. A lane that was saved again while
it was being checked is checked again by itself.

`batch` in `[verify]` says how many lanes a run may take (8 by default;
1 is one lane at a time, as before).

### A sandbox per lane

The checks of an integration run code that a lane wrote, as the user that
runs the daemon; so does anything an agent starts in its lane. Both run
confined (Linux 5.13 or later, with Landlock):

- they **write** only in their own copy of the files (the copy of the
  merged result for the checks, the lane's working copy for a command) and
  in a temporary directory of their own;
- they do **not read** Kito's own data (`.kito/`: the history, the other
  lanes, the token of a connected directory), the other repositories of a
  server and the server's own files, or the keys of the user that runs
  them (`~/.ssh`, `~/.aws`, `~/.gnupg` and a few more);
- the checks open **no TCP connection**; a command in a lane may.

Nor can they command the daemon: its socket asks for a key that is in
`.kito/`, which they cannot read.

`kito status` says how the checks of a repository run. A check that fails
confined says so after its output, since the confinement may be why. What
is allowed is in the repository's configuration:

```toml
[sandbox]
mode = "auto"        # "required": checks do not run where they cannot be confined; "off"
network = false      # the checks may open TCP connections
write = []           # more places they may write
hide = ["~/.ssh", "~/.aws", "~/.gnupg", "~/.config/gcloud", "~/.kube", "~/.docker", "~/.netrc", "~/.git-credentials"]
```

A command of your own, or of an agent, runs confined to a lane with:

```bash
kito lane run lane/3 -- npm test
```

It runs in the lane's working copy, and its exit code is the command's.
`--no-network` takes the network away too. Agents have the same as the
tool `run` of the MCP server.

Where the system cannot confine a command (macOS, an older kernel), the
checks run as before and `kito status` says that they are not confined;
`mode = "required"` refuses to run them instead, and `kito lane run`
always refuses.

## Development

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo bench -p kito-store -p kito-diff
```

Phase 14 demo: somebody commits with plain git and pushes while an agent
works on the same file; the commits are merged in and pushed back; one
that conflicts waits for a decision (needs `git`).

```bash
scripts/demo-phase14.sh
```

Phase 13 demo: five agents finish at the same time on a repository whose
checks take two seconds: one at a time, then together; a lane that breaks
the checks among four; two lanes on one file (needs `python3`).

```bash
scripts/demo-phase13.sh
```

Phase 12 demo: a server known by its certificate, what somebody between
a client and the server can read with and without encryption, and a lane
whose check tries to read another repository, write outside its copy and
call out (needs `python3`).

```bash
cargo build --release -p kitod -p kito-cli -p kito-server
```

```bash
scripts/demo-phase12.sh
```

Phase 11 demo: what Claude Code runs around its tools, run by hand: an
edit in the main directory and one outside a lane's scope are not made, an
edit is held back once for the reasons behind its lines, a shell command
is checked after it ran, and an integration waits until its reasons were
read (needs `python3`).

```bash
cargo build --release -p kitod -p kito-cli -p kito-mcp
```

```bash
scripts/demo-phase11.sh
```

Phase 10 demo: a repository with a history in git, a team working on it
through lanes, and the questions an agent asks before changing a function
(needs `python3`).

```bash
cargo build --release -p kitod -p kito-cli -p kito-server
```

```bash
scripts/demo-phase10.sh
```

Phase 9 demo: a direct edit to the main lane is refused, a failing test
stops a lane, one that passes waits for somebody who may integrate, and
the integration reaches a git remote (needs `python3`).

```bash
scripts/demo-phase9.sh
```

Phase 8 demo: an agent works in a lane on one machine while a person
follows it on another; the daemon is killed, the server goes away, and no
save is lost.

```bash
scripts/demo-phase8.sh
```

Phase 7 demo: one server, two repositories, two people; one opens a lane
and the other sees it.

```bash
scripts/demo-phase7.sh
```

Phase 6 demo: three agents declare what they will change, and the ones on
the same code are told before they write.

```bash
scripts/demo-phase6.sh
```

Phase 5 demo: the acceptance criteria of the plan measured on a repository
of 50,000 files, each with its verdict.

```bash
cargo build --release -p kitod -p kito-store -p kito-diff --examples
```

```bash
scripts/demo-phase5.sh
```

Phase 4 demo: a function renamed in one lane and newly used in another; git
merges them into something that does not compile, Kito carries the rename
to the new use (needs `tsc`, `node` and, for the automatic part,
`typescript-language-server`).

```bash
scripts/demo-phase4.sh
```

Phase 3 demo: two agents on the same file, a real conflict decided, then a
swarm of ten simulated agents verified by the TypeScript compiler.

```bash
cargo build --release -p kitod --example swarm
```

```bash
scripts/demo-phase3.sh
```

Phase 2 demo: a function is renamed, moved to another file and edited;
`kito why` tells its whole story.

```bash
scripts/demo-phase2.sh
```

Phase 1 demo: working without commits, `kito log` shows everything, `git log`
sees the checkpoints.

```bash
scripts/demo-phase1.sh
```

Phase 0 demo: one million ops, `kill -9` at random moments, no confirmed op
lost.

```bash
cargo run --release -p kito-store --example crash_demo -- driver --dir /tmp/kito-demo --ops 1000000
```

## Documents

- [`docs/phases.md`](docs/phases.md): what was done in each phase and how to rerun the demos.
- [`docs/decisions.md`](docs/decisions.md): choices made on points the plan does not settle.
- [`docs/benchmarks.md`](docs/benchmarks.md): measurements that justify the optimizations.
- [`docs/adr/`](docs/adr/): decisions that change the data model or a choice settled by the plan.
