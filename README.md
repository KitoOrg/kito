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

It also sets the project up for Claude Code (see [Claude Code](#claude-code);
`--no-claude` leaves that out).

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
kito why src/users.ts:fetchUser
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

### Claude Code

`kito init` and `kito import` set the project up for Claude Code, and
`kito claude` does it in a repository that is already there:

```bash
kito claude
```

| File | What goes in it |
| --- | --- |
| `.gitignore` | `.kito/`, so that git does not see Kito's own directory |
| `.mcp.json` | the `kito` server: `{ "command": "kito-mcp" }` |
| `.claude/settings.json` | `mcp__kito__*` among the allowed tools: Claude calls them without asking each time |
| `CLAUDE.md` | the rules: one lane per task, edits only in the lane's working copy, `rename_symbol` for renames, integrate at the end |

Each file is touched only for what is missing in it: what is already there
stays as it is written, and a second run changes nothing. A file that is not
what was expected (JSON that does not parse, say) is left alone and named.
They are files of the project: Kito records them, and they reach git with
the next checkpoint, so that whoever clones the project has them. To leave a
project as it is, `kito init --no-claude` (or `KITO_NO_CLAUDE=1` in the
environment).

Then start Claude Code in the project directory. The first time it asks to
trust the folder and to approve the `kito` server; `/mcp` shows it connected
with its eleven tools. `kito-mcp` has to be on the `PATH`. Claude Code
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
kito connect 127.0.0.1:7421 shop --token kito_...
```

```bash
kito watch
```

`kito watch` prints what happens as it happens: lanes opened and closed,
scopes extended, saves, integrations and how they end. A lane opened
through the server belongs to the person the client was authenticated as,
whatever the client says, and the board shows whose agent each lane is.

`serve` listens on this machine only, and what it carries is not
encrypted. From another machine the way in is SSH, where the person is who
SSH says they are:

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

Phase 9 demo: a direct edit to the main lane is refused, a failing test
stops a lane, one that passes waits for somebody who may integrate, and
the integration reaches a git remote (needs `python3`).

```bash
cargo build --release -p kitod -p kito-cli -p kito-server
```

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
