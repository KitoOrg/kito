//! Benchmark dello store (criterion). Ogni ottimizzazione di `kito-store` va
//! giustificata da un guadagno misurato qui (vedi `docs/decisions.md`).
//!
//! ```text
//! cargo bench -p kito-store
//! ```

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use kito_core::*;
use kito_store::{Store, StoreOptions};
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Duration;

const LANES: u64 = 4;

/// Contenuto simile a codice sorgente: comprimibile, lungo qualche centinaio di byte.
fn source(i: u64) -> Vec<u8> {
    let lines = 4 + (i * 7) % 20;
    (0..lines)
        .flat_map(|l| {
            format!("export function f{i}_{l}(x: number) {{ return x + {}; }}\n", i * l).into_bytes()
        })
        .collect()
}

struct Gen {
    n: u64,
    heads: [Option<OpId>; LANES as usize],
}

impl Gen {
    fn new() -> Gen {
        Gen { n: 0, heads: [None; LANES as usize] }
    }

    /// Un'op `TextEdit` con i suoi due blob: il caso tipico della cattura continua.
    fn next(&mut self) -> (Op, [Vec<u8>; 2]) {
        let i = self.n;
        self.n += 1;
        let lane = LaneId(i % LANES);
        let (a, b) = (source(i), source(i + 1));
        let kind = OpKind::TextEdit {
            file: PathBuf::from(format!("src/mod{}/f{}.ts", i % 50, i % 1000)),
            before: BlobId::of(&a),
            after: BlobId::of(&b),
            edits: vec![TextEdit::new(0, 16, format!("v{i}"))],
        };
        let parents: Vec<OpId> = self.heads[lane.0 as usize].into_iter().collect();
        let op = Op::new(parents, lane, Author::agent("bench"), Hlc::new(i, 0), kind, None).unwrap();
        self.heads[lane.0 as usize] = Some(op.id);
        (op, [a, b])
    }

    fn append(&mut self, store: &mut Store, n: usize) {
        for _ in 0..n {
            let (op, blobs) = self.next();
            for blob in &blobs {
                store.put_blob(blob).unwrap();
            }
            store.append_op(&op).unwrap();
        }
    }
}

fn filled(n: usize) -> (tempfile::TempDir, Store, Gen) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::init(dir.path()).unwrap();
    let mut gen = Gen::new();
    for _ in 0..n / 1000 {
        gen.append(&mut store, 1000);
        store.commit().unwrap();
    }
    (dir, store, gen)
}

fn write_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("write");
    group.sample_size(20).measurement_time(Duration::from_secs(8));

    // Throughput di scrittura in blocco: 1000 op (con 2 blob l'una) e un commit.
    group.throughput(Throughput::Elements(1000));
    group.bench_function("append_1000_ops_then_commit", |b| {
        let (_dir, mut store, mut gen) = filled(0);
        b.iter(|| {
            gen.append(&mut store, 1000);
            store.commit().unwrap();
        });
    });

    // Latenza di un salvataggio: una op, due blob, commit (fsync compresi).
    group.throughput(Throughput::Elements(1));
    group.bench_function("append_1_op_then_commit", |b| {
        let (_dir, mut store, mut gen) = filled(0);
        b.iter(|| {
            gen.append(&mut store, 1);
            store.commit().unwrap();
        });
    });

    for size in [512usize, 64 << 10] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(format!("put_blob_{size}_bytes"), |b| {
            let (_dir, mut store, _) = filled(0);
            let mut i = 0u64;
            b.iter(|| {
                i += 1;
                // Contenuto sempre diverso (niente deduplicazione), comprimibile.
                let mut data = source(i);
                while data.len() < size {
                    data.extend_from_slice(&source(i + data.len() as u64));
                }
                data.truncate(size);
                black_box(store.put_blob(&data).unwrap());
            });
        });
    }
    group.finish();
}

fn read_path(c: &mut Criterion) {
    const N: usize = 50_000;
    let (_dir, store, _) = filled(N);
    let mut ids = Vec::new();
    store
        .scan_ops(|_, op| {
            ids.push((op.id, op.kind.blobs()[0]));
            Ok(())
        })
        .unwrap();

    let mut group = c.benchmark_group("read");
    group.throughput(Throughput::Elements(1));
    let mut i = 0usize;
    group.bench_function("get_op", |b| {
        b.iter(|| {
            i = (i + 7919) % N;
            black_box(store.get_op(&ids[i].0).unwrap().unwrap());
        })
    });
    group.bench_function("get_blob", |b| {
        b.iter(|| {
            i = (i + 7919) % N;
            black_box(store.get_blob(&ids[i].1).unwrap().unwrap());
        })
    });
    group.bench_function("lane_ops_page_of_100", |b| {
        b.iter(|| {
            i = (i + 7919) % (N / LANES as usize - 100);
            black_box(store.lane_ops(LaneId(1), i as u64, 100).unwrap());
        })
    });
    group.finish();
}

fn recovery(c: &mut Criterion) {
    const N: usize = 50_000;
    let mut group = c.benchmark_group("recovery");
    group.sample_size(10).measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(N as u64));

    let (dir, store, _) = filled(N);
    store.close().unwrap();
    group.bench_function("open_clean_50k_ops", |b| {
        b.iter(|| black_box(Store::open(dir.path()).unwrap()));
    });
    group.bench_function("open_and_rebuild_index_50k_ops", |b| {
        b.iter_batched(
            || std::fs::remove_file(dir.path().join("index/index.redb")).unwrap(),
            |()| black_box(Store::open(dir.path()).unwrap()),
            BatchSize::PerIteration,
        );
    });
    group.bench_function("verify_50k_ops", |b| {
        let store = Store::open_with(dir.path(), StoreOptions::default()).unwrap();
        b.iter(|| assert!(store.verify().unwrap().is_ok()));
    });
    group.finish();
}

criterion_group!(benches, write_path, read_path, recovery);
criterion_main!(benches);
