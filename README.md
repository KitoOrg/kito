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
| 1 | Cattura continua testuale: `kitod`, CLI, export/import git | da fare |
| 2 | Semantica livello 1: tree-sitter, ID stabili, `kito why` | da fare |
| 3 | Lane e integrazione, server MCP | da fare |
| 4 | Semantica livello 2: LSP, conflitti `SemanticRef` | da fare |
| 5 | Copertura e prestazioni | da fare |

## Crate

| Crate | Responsabilità |
| --- | --- |
| `kito-core` | Tipi del modello dati, codifica CBOR canonica, ID e hash |
| `kito-store` | Pack dei blob, oplog a segmenti, indici ricostruibili, recupero dopo un crash |

Gli altri crate del piano nascono con la fase che li riempie.

## Usare il repository

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo bench -p kito-store
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
