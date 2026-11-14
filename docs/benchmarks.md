# Benchmarks

The plan asks to optimize only with a `criterion` benchmark that proves the
gain. The measurements that justified an optimization are recorded here.

They are run with:

```bash
cargo bench -p kito-store
```

The measurements below come from a 4-core VM with a virtual disk: they are
valid as a before/after comparison, not as absolute values.

## Phase 0 · reusing the zstd contexts

`zstd::bulk::compress` and `decompress` create a context on every call. For
blobs of a few hundred bytes that cost dominated everything else.

| Benchmark | Before | After | Change |
| --- | --- | --- | --- |
| `write/put_blob_512_bytes` | 24.8 µs | 9.4 µs | −62% |
| `read/get_blob` | 20.4 µs | 4.1 µs | −81% |
| `write/append_1000_ops_then_commit` (1000 ops, 2000 blobs) | 50.7 ms | 26.4 ms | −41% |
| `write/put_blob_65536_bytes` | 445 µs | 388 µs | within noise |

`Packs` now holds a `Compressor` and every reader thread a `Decompressor`.

## Phase 0 · reference values

| Benchmark | Measurement |
| --- | --- |
| `write/append_1_op_then_commit` (1 op, 2 blobs, 2 fsyncs) | 0.56 ms |
| `read/get_op` | 3.7 µs |
| `read/lane_ops_page_of_100` | 172 µs |
| `recovery/open_clean_50k_ops` | 6.4 ms |
| `recovery/open_and_rebuild_index_50k_ops` | 281 ms |
| `recovery/verify_50k_ops` | 1.2 s (before the reuse of the zstd contexts) |

## Phase 1 · save → op latency

End-to-end measurement with the real daemon, a 2,000-line file (172 kB), one
line changed on every save, 300 saves:

```bash
cargo run --release -p kitod --example latency -- --saves 300 --lines 2000
```

| Measurement | p50 | p95 | max |
| --- | --- | --- | --- |
| Save → confirmed op, as seen by the client | 54.9 ms | 56.7 ms | 91.1 ms |
| First event → confirmed op, measured by the daemon | 53.8 ms | 55.5 ms | |
| Processing alone, after the debounce | 3.1 ms | 4.7 ms | |

The debounce is 50 ms. See the discussion of the "< 30 ms at p95" target in
[`phases.md`](phases.md).

## Phase 2 · the cost of symbols on every save

```bash
cargo bench -p kito-diff
```

The first version parsed both sides of every save from scratch and looked up
each old symbol with a scan of the table: on a file of 2,000 functions the
processing of a save went from 3 ms (phase 1) to 127 ms.

| Step, file of 2,000 functions (172 kB), one changed | Before | After | What changed |
| --- | --- | --- | --- |
| Matching old and new symbols (`diff_one_function_changed_of_2000`) | 33.4 ms | 1.1 ms | old symbols looked up by (parent, kind, name) instead of scanning |
| Reading the new side (`reparse_2000_functions_one_changed`) | 42.8 ms | 21.8 ms | incremental parse from the cached tree of the old side |
| Reading the old side | 42.8 ms | 0 | taken from the parse cache |

`parse_2000_functions` (from scratch, 42.8 ms) is the reference for the first
touch of a file. Of the 21.8 ms of an incremental reparse, about 1 ms is the
parse itself: the rest is running the symbols query and hashing the tokens
of the whole file again, which is what a later optimization can avoid.

End to end, with the real daemon (`--shape typical` is an ordinary source
file, `--shape dense` the worst case of one function per line):

```bash
cargo run --release -p kitod --example latency -- --shape typical
```

| File | Processing p50 | p95 |
| --- | --- | --- |
| 2,000 lines, 143 functions (58 kB) | 14.1 ms | 20.9 ms |
| 2,000 lines, 2,000 functions (172 kB) | 31.5 ms | 50.5 ms |

