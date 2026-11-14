# Stato delle fasi

Una fase alla volta: ognuna si chiude con test verdi e una demo eseguibile.

## Fase 0 · fondamenta — completata

`kito-core` e `kito-store`: oplog append-only con CRC, objects BLAKE3 + zstd,
indici ricostruibili.

### Cosa c'è

- **Modello dati** (`kito-core`): i tipi del piano, con ID e hash; codifica
  CBOR canonica; `OpId` = BLAKE3 di (parents, kind, author); clock HLC. Un
  test "golden" fissa i byte e l'ID di un'op di esempio: se cambia, il formato
  su disco è cambiato e serve un ADR.
- **Log a segmenti** (`kito-store`): record `lunghezza + payload + CRC32C`,
  segmenti da 64 MiB creati in modo atomico (scrittura, fsync, rename), un
  solo scrittore protetto da lock.
- **Blob**: indirizzati con BLAKE3, compressi con zstd se conviene,
  deduplicati, verificati contro l'hash a ogni lettura.
- **Indici** (redb): op → posizione e lane, lane → op, simbolo → op, blob →
  posizione. Sempre ricostruibili dai log.
- **Recupero**: all'apertura la coda troncata viene scartata (e conservata in
  `lost+found/`), l'indice riportato in pari. Un danno a dati già confermati
  non viene mai nascosto: è un errore, superabile solo in modalità riparazione.
- **`Store::verify`**: controllo completo di CRC, hash, ID, genitori, indici.

### Demo

Scrivere 1 milione di op, fare `kill -9` in momenti casuali, riavviare e
verificare che nessuna op confermata sia persa.

```bash
cargo run --release -p kito-store --example crash_demo -- driver --dir /tmp/kito-demo --ops 1000000
```

Esito dell'esecuzione del 14 novembre 2026 (seed 20261114, output in
[`demo/fase0-crash-demo.txt`](demo/fase0-crash-demo.txt)):

| | |
| --- | --- |
| Op scritte e verificate una per una | 1.000.000 |
| `kill -9` | 211 |
| …dopo un commit | 95 |
| …all'avvio, anche durante il recupero | 40 |
| …a metà di una scrittura | 76 |
| Code troncate scartate | 33 (29.768 byte, mai confermati) |
| Indice riportato in pari dopo un kill | 113 volte |
| Op confermate perse | 0 |
| Durata | 2 min 38 s |

Alla fine la demo rilegge tutte le op e i blob, esegue `verify` e ricostruisce
l'indice da zero controllando che risponda come quello incrementale.

### Test

- Proprietà (`proptest`): i record sopravvivono a qualunque troncamento e
  rilevano qualunque bit invertito; la codifica delle op è deterministica.
- Modello con perdita di corrente simulata: sequenze casuali di scritture,
  commit, crash, fsync falliti; i byte non sincronizzati vengono persi,
  troncati, azzerati o sporcati. Dopo ogni riapertura lo store contiene un
  prefisso della storia con almeno tutte le op confermate.
- Fault injection: troncamento a ogni byte della coda, spazzatura in fondo ai
  file, bit invertiti in segmenti sigillati e attivi, segmenti mancanti, disco
  pieno in punti diversi, fsync fallito, scrittura parziale non annullabile,
  indice mancante, illeggibile, vecchio o di un altro store.

### Criteri di accettazione toccati da questa fase

| Metrica | Obiettivo v1 | Stato |
| --- | --- | --- |
| Recupero dopo un crash | Nessuna op confermata persa | verificato dalla demo e dai test |

Le latenze si misurano dalla fase 1, quando esiste il percorso completo
salvataggio → op nel log. Come riferimento, il solo store impiega circa 0,6 ms
per confermare un'op con due blob (vedi [`benchmarks.md`](benchmarks.md)).

### Limiti noti

- La demo usa `kill -9`, che non perde la cache del sistema operativo: la
  perdita di corrente è coperta solo dalla simulazione nei test.
- Vedi D-007 e D-011 in [`decisions.md`](decisions.md).

## Fase 1 · cattura continua testuale — da fare

## Fase 2 · semantica livello 1 — da fare

## Fase 3 · lane e integrazione — da fare

## Fase 4 · semantica livello 2 — da fare

## Fase 5 · copertura e prestazioni — da fare
