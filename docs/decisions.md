# Decisioni non coperte dal piano

Scelte fatte durante l'implementazione su punti che il piano di progetto non
fissa. Regola del piano: l'opzione più semplice e reversibile, annotata qui.
Le decisioni che cambiano la semantica di un tipo del modello dati vanno
invece in un ADR (`docs/adr/`).

## Fase 0

### D-001 · Codifica binaria: CBOR
I record su disco e l'input degli hash di contenuto sono in CBOR (RFC 8949,
crate `ciborium`). È autodescrittivo: le struct sono mappe con i nomi dei
campi. Il piano dice che al modello dati si possono aggiungere campi; con CBOR
un campo opzionale nuovo, omesso quando è assente, non rompe la lettura dei
record vecchi e non cambia gli hash già calcolati. Un formato posizionale
(postcard, bincode) sarebbe più compatto e veloce ma renderebbe ogni campo
aggiunto una migrazione. Un'op tipica occupa circa 300 byte.
*Reversibile:* la codifica è isolata in `kito_core::codec`; l'header di ogni
file di log porta un numero di versione del formato.

### D-002 · `OpId` copre solo (parents, kind, author)
Come da piano: "hash di (parents, kind, payload, author)". `lane`, `at` e
`intent` restano fuori: sono metadati, e la stessa modifica dello stesso autore
sugli stessi genitori è la stessa op. Gli hash strutturati usano BLAKE3 in
modalità `derive_key` con un contesto per tipo, così un `OpId` non può
collidere con un `BlobId`. I blob usano BLAKE3 semplice (coincide con `b3sum`).

### D-003 · Tipi che il piano nomina senza definire
`LaneId(u64)` (0 = linea principale), `CheckpointId(u64)` e `ConflictId(u64)`
sono contatori locali al repository; `Author { kind: Agent | Human, name }`;
`Hlc { wall_ms, logical }`; `Location { file, line, start, end }` con offset in
byte; `TextEdit { start, end, text }` con intervalli riferiti al testo di
partenza; `Verification { commands, passed, duration_ms }`;
`Fingerprint { sketch: Vec<u32> }`, uno sketch MinHash la cui somiglianza è la
frazione di posizioni uguali (l'algoritmo che lo calcola arriva con la fase 2).

### D-004 · Percorsi UTF-8
I `PathBuf` del modello dati sono relativi alla radice del repository e devono
essere UTF-8 validi: un percorso che non lo è non si serializza. I file con
nomi non UTF-8 verranno saltati con un avviso dalla cattura (fase 1).

### D-005 · Un solo oplog globale, non uno per lane
`oplog/` è un unico log a segmenti; l'indice `lane → op` dà la vista per lane.
È il layout del piano e rende banale l'ordine totale di registrazione.

### D-006 · Cosa significa "confermata"
Le scritture si accodano in memoria; `Store::commit` le rende durevoli. Un'op è
confermata quando il `commit` che la segue restituisce `Ok`. L'ordine dentro
`commit` è obbligato: fsync dei pack, poi scrittura e fsync dell'oplog, poi
indice. Il buffer dell'oplog non tocca il file prima che i pack siano durevoli,
quindi un'op presente su disco non cita mai un blob assente, nemmeno dopo una
perdita di corrente.

### D-007 · Coda troncata e dati confermati danneggiati sono casi diversi
All'apertura il file attivo di ogni log viene riletto per intero. Il primo
record non valido e tutto ciò che lo segue vengono scartati (dopo averne
salvato una copia in `lost+found/`): è il "record finale troncato" del piano.
Ma l'indice registra fin dove i log erano già confermati, e sotto quella soglia
il recupero non scarta nulla: restituisce `StoreError::Corrupt`. Per accettare
la perdita serve aprire con `StoreOptions::repair`. I segmenti sigillati non
vengono mai troncati: un loro record danneggiato è un errore alla lettura e in
`Store::verify`. Senza questa distinzione un bit rovinato a metà del segmento
attivo farebbe sparire in silenzio tutte le op confermate successive.
*Limite noto:* tra una sincronizzazione dell'indice e l'altra (1 secondo di
default) la soglia è indietro, e un danno in quella finestra viene trattato
come coda troncata.

### D-008 · Errore di scrittura = store avvelenato
Dopo un errore di I/O in scrittura (disco pieno, fsync fallito) lo stato dei
file è incerto: lo store rifiuta ogni altra scrittura con `Poisoned` e va
riaperto. Le letture restano possibili. È più semplice che tentare un rollback
in memoria, e il recupero all'apertura è già il percorso collaudato.

### D-009 · Indice redb con commit non durevoli
L'indice si aggiorna a ogni commit ma fa fsync solo ogni
`index_sync_interval` (1 s) e alla chiusura. Dopo un crash può essere indietro
rispetto ai log: l'apertura rilegge la coda dei log e lo riporta in pari. Se
manca, è illeggibile, è più avanti dei log o non corrisponde a essi (si
confronta il CRC dell'ultima op indicizzata), viene ricostruito da zero.

### D-010 · Limiti di dimensione
Un record porta al massimo 2 GiB − 1 di payload, quindi un blob non può
superare quella dimensione (i file binari grandi sono fuori scope per la v1).
Un blob più grande di un pack ottiene un pack tutto suo.

### D-011 · Lettori in altri processi: rimandati
Le letture passano da `mmap` dentro il processo che possiede lo store. Un
lettore in un altro processo (la CLI che legge i file senza passare dal
daemon) non è previsto in questa fase: l'architettura fa passare CLI e MCP
dal daemon. Se servirà, andrà aggiunto un lock condiviso che escluda il
troncamento del recupero.

### D-012 · I crate nascono con la loro fase
Il workspace arriverà a 12 crate; ognuno viene creato quando comincia la fase
che lo riempie, invece di tenere crate vuoti.

## Fase 1

### D-013 · Tipo dei file (`FileMode`)
`FileAdd` ha un campo opzionale `mode` (normale, eseguibile, link simbolico),
assente dalla codifica quando è "normale". Senza, l'export su git perderebbe
il bit di esecuzione e la CI smetterebbe di funzionare. È un campo aggiunto,
non un cambio di semantica: gli hash dei `FileAdd` esistenti non cambiano. Il
modello non ha un'op per il solo cambio di tipo, quindi un `chmod +x` è un
`FileRemove` seguito da un `FileAdd`.

### D-014 · L'albero si ricostruisce solo dalle op
`Tree` (percorso → blob) si ottiene riapplicando le op, senza mai rieseguire
un parser: la stessa storia dà lo stesso albero anche con versioni future
delle grammatiche. Conseguenza per la fase 2: le op simboliche dovranno
portare con sé anche l'effetto sul file (blob prima e dopo), oppure essere
accompagnate da un'op che lo fa.

### D-015 · Edit in linea solo se piccoli
`TextEdit.edits` contiene gli edit esatti (diff per righe) quando il file è
testo e la modifica pesa meno di 32 KiB; altrimenti è vuoto, e la modifica
resta descritta dai due blob. Evita di scrivere due volte i contenuti grandi.

### D-016 · Rename solo esatti
Un file sparito e uno comparso nello stesso gruppo, con contenuto identico
(non vuoto) e stesso tipo, sono un `FileRename`. Nessuna euristica di
somiglianza: un falso rename inquina la storia. Rename con modifica nello
stesso salvataggio diventano rimozione più aggiunta.

### D-017 · Export su git
- I commit sono deterministici: autore dal checkpoint, data dall'ultima op,
  messaggio = etichetta più `Kito-Checkpoint: ckpt#N`. Indirizzo email
  `<autore>@kito.invalid`, configurabile.
- Il primo export in un repository git che ha già commit li prende come
  genitori: la storia continua.
- Il ramo avanza solo in fast-forward. Se punta a un commit che Kito non
  conosce, l'export si ferma (`Diverged`). Se era rimasto indietro per un
  export interrotto, viene riallineato.
- Dopo l'export l'indice di git viene riscritto dall'ultimo albero.

### D-018 · Import da git
Si importa la catena dei primi genitori di HEAD: ogni commit diventa le op
che portano dall'albero del genitore al suo, più un checkpoint che ricorda il
commit (e quindi non viene riesportato). I submodule e i percorsi non UTF-8
si saltano, elencandoli. `kito import` crea `.kito/` se manca.

### D-019 · Protocollo tra daemon e client
JSON-RPC 2.0, un messaggio per riga, su socket Unix in `.kito/kitod.sock`
(in `/tmp` se il percorso supera il limite dei socket). È il default indicato
tra le questioni aperte del piano; un protocollo binario resta possibile
perché il trasporto è isolato in `kitod::rpc` e `kitod::client`.

### D-020 · `sync` è una scansione completa
La barriera non si fida degli eventi: confronta tutta la cartella di lavoro
con l'albero (con la cache degli stat costa uno `stat` per file). Quando
ritorna, ciò che era su disco è nel log, anche se il watcher ha perso eventi.

### D-021 · Un file tracciato resta tracciato
Come in git, una regola di ignore aggiunta dopo non fa sparire dal log un
file già tracciato; vale per i file nuovi.

### D-022 · `kito init` avvia il daemon e non scavalca git
La cattura è continua solo se il daemon gira: `init` e `import` lo avviano.
In una cartella che è già un repository git `init` si rifiuta e indica
`kito import` (che ne porta dentro la storia) o `init --fresh`.

### D-023 · Comandi in più rispetto al piano
`kito sync` (la barriera, che il piano prevede solo come tool MCP),
`kito status` e `kito daemon start|stop`.

### D-024 · Stato salvato della linea principale
`.kito/lanes/main/state` contiene l'albero e la cache degli stat per
ripartire in fretta. È un derivato: se manca, è illeggibile o non corrisponde
al log, si riapplicano le op.

### D-025 · Quando gli eventi dei file non arrivano
Su alcuni file system il sistema operativo non segnala tutte le modifiche:
il caso tipico è `/mnt/c` sotto WSL, dove ciò che cambia un programma Windows
non genera eventi in Linux, ma vale anche per le cartelle di rete. Tre misure:

- `kito log` e `kito status` eseguono la barriera (`sync`) prima di
  rispondere: ciò che mostrano riflette sempre il disco, a prescindere dal
  watcher.
- Se una scansione trova modifiche mentre il watcher tace da più di un
  secondo, il daemon conclude che lì gli eventi non bastano e passa alla
  scansione frequente: al più una al secondo, e spendendo in scansioni non
  più di un ventesimo del tempo. Parte direttamente così sui file system noti
  per questo problema (9p, drvfs, cifs, nfs…) e con `capture.watch = false`.
- `kito daemon status` dice quale intervallo è in uso e perché; `init` e
  `import` avvisano subito.

Con la scansione al posto degli eventi la cattura resta completa ma meno
fine: i salvataggi tra una scansione e l'altra si fondono in una sola op, e
l'ora registrata è quella della scansione.

### D-026 · Identity
Who signs captured changes is resolved, in order, from: the repository
config (`[author]` in `.kito/config.toml`), the environment (`KITO_AUTHOR`,
`KITO_EMAIL`), the personal config (`~/.config/kito/config.toml`), git's
`user.name`/`user.email`, and finally `$USER`. Name and email are resolved
independently, as git does. `Author` gained an optional `email` field (absent
from the encoding when unset, so existing op IDs are unchanged); exported git
commits use it, which is what lets GitHub attribute them. `kito identity`
shows the identity in use and where it comes from, and sets it
(`--name`, `--email`, `--agent`/`--human`, `--global`).

### D-027 · Rollback never rewrites history
`kito rollback <point> [paths…]` brings the working directory (or only the
given paths) back to the state at a checkpoint, at an op, or N ops ago. The
log stays append-only: the current state is captured first, then the files
are rewritten and the difference is recorded as new ops carrying an intent
("rollback to ckpt#1"). A rollback can therefore always be undone by rolling
back to the previous head, which the command prints. An untracked (ignored)
file sitting where a file must be recreated is never overwritten: the
rollback refuses instead.

### D-028 · Intents live in their own append-only log
`.kito/intents` stores `Intent` records like `.kito/checkpoints` stores
checkpoints. An op may only cite an intent the store already knows, and
`Store::verify` reports ops whose intent is missing.
