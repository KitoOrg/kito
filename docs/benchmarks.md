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

## Phase 3 · integration and lane creation on a large repository

```bash
cargo run --release -p kitod --example scale -- --files 50000
```

A repository of 50,000 TypeScript files of 1 kB (250,000 symbols). A lane
changes one function in each of 50 files, which is 100 ops (a file-level op
and a symbolic one per file), and is integrated; the verifier is off.

| On 50,000 files | Before | After |
| --- | --- | --- |
| Integrating 100 ops | 2,119 ms | 90 ms |
| Integrating 1,000 ops | 3,753 ms | 299 ms |
| Integrating 100 ops from a lane the main lane left 550–650 files behind | 754–919 ms (measured halfway through the work) | 123–136 ms |
| Creating a lane (plain copies, ext4) | 1,433–1,840 ms | 462–561 ms |

Where the 2,119 ms of the first row went, step by step:

| Step | Before | After | What changed |
| --- | --- | --- | --- |
| Scanning the lane's working copy | 168 ms | 60 ms | the file system is asked about the files by four threads; only what the stat cache does not vouch for is looked at |
| Scanning the main directory, before the merge | 177 ms | 3 ms | only the files the lane touched are looked at (D-048) |
| Merge | 177 ms | 5 ms | reads only the files involved; the three versions come from the parse cache |
| Scanning both again, before writing | 358 ms | 2 ms | only the files about to be written |
| Writing and recording the merged files | 46 ms | 20–30 ms | — |
| Saving the state of the main lane | 250 ms | 0 | saved when the daemon stops (D-048) |
| Bringing the lane to the new state | 941 ms | 0.5 ms | `Tree` and `SymbolTable` are persistent maps: the copies of the state cost nothing (D-052); the lane's state is not saved here |

The parse cache went from 64 to 1,024 entries in the same work. On 5,000
files, same build except for the constant:

| Integrating | 64 entries | 1,024 entries |
| --- | --- | --- |
| 100 ops (50 files) | 71–96 ms | 37–38 ms |
| 1,000 ops (500 files) | 1,034–1,092 ms | 316–318 ms |

A save in a lane or in the main directory costs what it did: the phase 2
code and this one, run one after the other on the same machine, both process
a save of the ordinary 2,000-line file in 12–15 ms at p50 and 19–21 ms at
p95.

### Creating a lane

```bash
cargo run --release -p kito-lane --example lane_creation -- --files 50000
```

No file system with reflinks was available on the test machine, so the
figures are for the fallback: every file copied. Per file, the first version
tried to clone, failed, removed what the attempt left and copied (about 20
system calls); now the first failure settles the matter for the whole lane,
and a small file is one read and one write (10 system calls).

These figures vary more than the others. On this VM (ext4 mounted with
`discard`, block bitmaps read on first use) creating a file costs from 13 µs
to 250 µs depending on what the file system has in memory, and the same
50,000-file lane takes from 0.5 s to 2 s, sometimes more, on consecutive
runs of the same binary. The "after" above is a quiet run.
