# ADR (Architecture Decision Record)

Serve un ADR, scritto **prima** del codice, per:

- cambiare la semantica di un tipo del modello dati (`kito-core`): aggiungere
  campi opzionali in coda non richiede ADR, tutto il resto sì;
- cambiare una delle scelte della sezione "Decisioni prese" del piano di
  progetto (linguaggio, rapporto con git, hash, interfacce…);
- cambiare il formato su disco in modo incompatibile (il test
  `golden_encoding_is_stable` di `kito-core` fallisce).

Le scelte su punti che il piano non copre non sono ADR: vanno in
[`../decisions.md`](../decisions.md).

## Formato

File `NNNN-titolo-breve.md`, numerati in ordine:

```markdown
# NNNN · Titolo

Stato: proposto | accettato | superato da NNNN
Data: AAAA-MM-GG

## Contesto
Cosa non funziona con la scelta attuale.

## Decisione
Cosa cambia, con i tipi o i formati interessati.

## Conseguenze
Compatibilità con i dati esistenti, migrazione, cosa diventa più difficile.
```

Nessun ADR finora.
