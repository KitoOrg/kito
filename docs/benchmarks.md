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
