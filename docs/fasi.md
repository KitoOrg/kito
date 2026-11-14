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

## Fase 1 · cattura continua testuale — completata

`kitod` con watcher; op `TextEdit` e `File*`; checkpoint; `kito log`; export
su git e import da git.

### Cosa c'è

- **Cattura** (`kitod`): un watcher sulla cartella di lavoro raccoglie gli
  eventi, attende 50 ms di quiete e trasforma lo stato dei file in op
  concatenate sulla linea principale. Una scansione completa riconcilia
  all'avvio, ogni 60 secondi e quando il watcher segnala eventi persi; usa una
  cache di dimensione e data per non rileggere i file invariati.
- **Op testuali** (`kito-diff`): aggiunta, rimozione, rename (solo se il
  contenuto è identico) e modifica con gli edit per righe.
- **Regole di ignore**: `.gitignore` anche annidati, `.git/info/exclude`,
  `.kitoignore` e la configurazione; le stesse per watcher e scansione.
- **Checkpoint**: etichette su un punto del log, create a mano.
- **Git** (`kito-git`): ogni checkpoint diventa un commit deterministico; il
  ramo avanza solo in fast-forward e l'indice di git viene riallineato, così
  `git status` resta pulito. L'import porta dentro la storia di un repository
  esistente.
- **Daemon e CLI**: socket Unix con JSON-RPC, un solo daemon per repository,
  avviato da sé dalla CLI. Comandi: `init`, `import`, `log`, `checkpoint`,
  `export-git`, `sync`, `status`, `daemon status|start|stop`; tutti con `--json`.

### Demo

Lavori in una cartella senza fare commit, `kito log` mostra tutto, `git log`
vede i checkpoint.

```bash
cargo build --release -p kitod -p kito-cli
```

```bash
scripts/demo-fase1.sh
```

Output dell'esecuzione del 14 novembre 2026 in
[`demo/fase1-demo.txt`](demo/fase1-demo.txt). In breve: tre salvataggi senza
alcun commit compaiono in `kito log` come `FileAdd` e `TextEdit`; due
`kito checkpoint` diventano due commit in `git log`; `git status` è pulito e
`git fsck --strict` passa.

### Criteri di accettazione ricontrollati

Il piano chiede di ricontrollare gli obiettivi con misure reali alla fine di
questa fase. L'unico misurabile ora è la latenza di un salvataggio; creazione
delle lane e integrazione arrivano con la fase 3.

Salvataggio di un file da 2.000 righe (172 kB), 300 salvataggi, daemon vero:

| Misura | p50 | p95 | Obiettivo v1 |
| --- | --- | --- | --- |
| Elaborazione dopo il debounce (lettura, hash, diff, due fsync) | 3,1 ms | 4,7 ms | |
| Dal primo evento all'op confermata, debounce compreso | 53,8 ms | 55,5 ms | |
| Dal salvataggio all'op confermata, vista dal client | 54,9 ms | 56,7 ms | < 30 ms al p95 |

**L'obiettivo non è raggiungibile così com'è scritto**: il piano fissa un
debounce di 50 ms, che da solo supera i 30 ms. Il lavoro vero costa meno di
5 ms. Le strade sono due, e la scelta spetta a chi possiede il piano:
intendere l'obiettivo come "dopo il debounce" (già rispettato con ampio
margine), oppure abbassare `debounce_ms` a 20 o meno in `.kito/config.toml`
(la latenza totale scende a circa 25 ms, al prezzo di qualche op in più
quando un editor salva in più passi). Il default resta 50 ms.

La misura si rifà con:

```bash
cargo run --release -p kitod --example latency -- --saves 300 --lines 2000
```

### Test

- Motore di cattura: ogni tipo di cambiamento su disco, eventi per percorso
  e per cartella, regole di ignore, riscritture della stessa dimensione,
  ripartenza dopo un crash, modifiche fatte a daemon spento.
- Daemon vero: cattura senza richieste esplicite, raffiche di salvataggi,
  `sync` come barriera, riavvio, un solo daemon per repository, `kill -9`
  durante la cattura.
- Git: export verificato con `git fsck --strict` e confrontato file per file
  con lo stato di Kito, anche su sequenze casuali di stati; continuità con una
  storia git preesistente; rilevamento dei commit fatti fuori da Kito; import
  con rename, permessi, merge.
- CLI: i comandi veri, in formato leggibile e JSON.

### Limiti noti

- L'import segue solo i primi genitori: i rami fusi compaiono come effetto
  del commit di merge (D-018).
- I commit fatti direttamente in git dopo l'import non vengono ancora
  riassorbiti: l'export si ferma con un errore chiaro (D-017).
- Gli intent (il "perché" delle op) arrivano con la fase 3, insieme al tool
  MCP che li imposta.
- Solo Linux e macOS, come da default del piano; provato su Linux.
- Dove gli eventi dei file non arrivano (per esempio `/mnt/c` sotto WSL) la
  cattura passa per la scansione: completa, ma con circa un secondo di
  ritardo e senza i salvataggi intermedi (D-025).

## Fase 2 · semantica livello 1 — da fare

## Fase 3 · lane e integrazione — da fare

## Fase 4 · semantica livello 2 — da fare

## Fase 5 · copertura e prestazioni — da fare
