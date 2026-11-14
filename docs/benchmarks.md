# Benchmark

Il piano chiede di ottimizzare solo con un benchmark `criterion` che dimostri
il guadagno. Qui si annotano le misure che hanno giustificato un'ottimizzazione.

Si lanciano con:

```bash
cargo bench -p kito-store
```

Le misure sotto vengono da una VM a 4 core con disco virtuale: valgono come
confronto prima/dopo, non come valori assoluti.

## Fase 0 · riuso dei contesti zstd

`zstd::bulk::compress` e `decompress` creano un contesto a ogni chiamata. Per
blob di poche centinaia di byte quel costo dominava tutto il resto.

| Benchmark | Prima | Dopo | Variazione |
| --- | --- | --- | --- |
| `write/put_blob_512_bytes` | 24,8 µs | 9,4 µs | −62% |
| `read/get_blob` | 20,4 µs | 4,1 µs | −81% |
| `write/append_1000_ops_then_commit` (1000 op, 2000 blob) | 50,7 ms | 26,4 ms | −41% |
| `write/put_blob_65536_bytes` | 445 µs | 388 µs | entro il rumore |

Ora `Packs` tiene un `Compressor` e ogni thread lettore un `Decompressor`.

## Fase 0 · valori di riferimento

| Benchmark | Misura |
| --- | --- |
| `write/append_1_op_then_commit` (1 op, 2 blob, 2 fsync) | 0,56 ms |
| `read/get_op` | 3,7 µs |
| `read/lane_ops_page_of_100` | 172 µs |
| `recovery/open_clean_50k_ops` | 6,4 ms |
| `recovery/open_and_rebuild_index_50k_ops` | 281 ms |
| `recovery/verify_50k_ops` | 1,2 s (prima del riuso dei contesti zstd) |

## Fase 1 · latenza salvataggio → op

Misura end-to-end con il daemon vero, file da 2.000 righe (172 kB), una riga
cambiata a ogni salvataggio, 300 salvataggi:

```bash
cargo run --release -p kitod --example latency -- --saves 300 --lines 2000
```

| Misura | p50 | p95 | massimo |
| --- | --- | --- | --- |
| Salvataggio → op confermata, vista dal client | 54,9 ms | 56,7 ms | 91,1 ms |
| Primo evento → op confermata, misurata dal daemon | 53,8 ms | 55,5 ms | |
| Sola elaborazione, dopo il debounce | 3,1 ms | 4,7 ms | |

Il debounce è di 50 ms. Vedi la discussione dell'obiettivo "< 30 ms al p95"
in [`fasi.md`](fasi.md).
