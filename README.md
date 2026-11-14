# Kito

Version control system semantico, in Rust, per sciami di agenti AI.

L'unità di storia è l'operazione semantica su un simbolo, catturata in
continuo: decine di agenti lavorano in parallelo sullo stesso repository e
incontrano quasi solo conflitti reali. Ogni checkpoint sulla linea principale
si esporta come commit git.

Il progetto si costruisce per fasi; ogni fase si chiude con test verdi e una
demo eseguibile.

| Fase | Contenuto | Stato |
| --- | --- | --- |
| 0 | Fondamenta: `kito-core`, `kito-store` | completata |
| 1 | Cattura continua testuale: `kitod`, CLI, export/import git | completata |
| 2 | Semantica livello 1: tree-sitter, ID stabili, `kito why` | da fare |
| 3 | Lane e integrazione, server MCP | da fare |
| 4 | Semantica livello 2: LSP, conflitti `SemanticRef` | da fare |
| 5 | Copertura e prestazioni | da fare |

## Crate

| Crate | Responsabilità |
| --- | --- |
| `kito-core` | Tipi del modello dati, codifica CBOR canonica, ID e hash |
| `kito-store` | Pack dei blob, oplog a segmenti, indici ricostruibili, recupero dopo un crash |
| `kito-diff` | Diff testuale e produzione delle op sui file (il differ AST arriva con la fase 2) |
| `kito-git` | Export dei checkpoint come commit git, import da un repository git |
| `kitod` | Daemon: watcher, cattura, socket JSON-RPC |
| `kito-cli` | Binario `kito` |

Gli altri crate del piano nascono con la fase che li riempie.

## Provarlo

```bash
cargo build --release -p kitod -p kito-cli
```

```bash
export PATH="$PWD/target/release:$PATH"
```

In una cartella nuova (in un repository git esistente usa `kito import`):

```bash
kito init
```

Da qui ogni salvataggio viene registrato, senza commit. Per vedere cosa è
successo e fissare un punto, che diventa anche un commit git:

```bash
kito log
```

```bash
kito checkpoint -m "prima versione"
```

## Sviluppo

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo bench -p kito-store
```

Demo della fase 1: lavorare senza commit, `kito log` mostra tutto, `git log`
vede i checkpoint.

```bash
scripts/demo-fase1.sh
```

Demo della fase 0: un milione di op, `kill -9` in momenti casuali, nessuna op
confermata persa.

```bash
cargo run --release -p kito-store --example crash_demo -- driver --dir /tmp/kito-demo --ops 1000000
```

## Documenti

- [`docs/fasi.md`](docs/fasi.md): cosa è stato fatto in ogni fase e come rifare le demo.
- [`docs/decisions.md`](docs/decisions.md): scelte fatte su punti che il piano non fissa.
- [`docs/benchmarks.md`](docs/benchmarks.md): misure che giustificano le ottimizzazioni.
- [`docs/adr/`](docs/adr/): decisioni che cambiano il modello dati o una scelta fissata dal piano.
