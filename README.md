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
| 3 | Lanes and integration, MCP server | to do |
| 4 | Level 2 semantics: LSP, `SemanticRef` conflicts | to do |
| 5 | Coverage and performance | to do |

## Crates

| Crate | Responsibility |
| --- | --- |
| `kito-core` | Data model types, canonical CBOR encoding, IDs and hashes |
| `kito-store` | Blob packs, segmented oplog, rebuildable indexes, crash recovery |
| `kito-parse` | tree-sitter grammars and symbol queries for the ten languages, normalized hashes |
| `kito-diff` | Textual diff, file ops, and the AST differ that keeps symbol IDs stable |
| `kito-git` | Export of checkpoints as git commits, import from a git repository |
| `kitod` | Daemon: watcher, capture, JSON-RPC socket |
| `kito-cli` | The `kito` binary |

The other crates in the plan are created with the phase that fills them.

## Trying it out

```bash
cargo build --release -p kitod -p kito-cli
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
