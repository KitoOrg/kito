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
| `kitod` | Daemon: watcher, capture on every lane, integration, JSON-RPC socket |
| `kito-cli` | The `kito` binary |
| `kito-mcp` | The MCP server agents talk to |

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

From here on every save is recorded, with no commits. To see what happened
and mark a point, which also becomes a git commit:

```bash
kito log
```

```bash
kito checkpoint -m "first version"
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
recorded like yours. Opening one prints where it is:

```bash
kito lane open alice --task "task #12: taxes"
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

If the client does not accept a dot in tool names, add
`"--tool-prefix", "kito_"`.

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
