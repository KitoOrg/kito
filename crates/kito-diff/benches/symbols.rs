//! Benchmarks of semantic level 1: parsing a file and comparing two
//! versions of it. They are what the save → op latency is made of once a
//! file has symbols.
//!
//! ```text
//! cargo bench -p kito-diff
//! ```

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use kito_core::{LaneId, SymbolTable};
use kito_diff::ast::{diff_symbols, FileChange, Version};
use kito_parse::{parse, parse_reusing, Dialect};
use std::hint::black_box;
use std::path::Path;

/// A TypeScript module of `functions` small functions, one of which differs
/// with `variant`.
fn module(functions: usize, variant: usize) -> String {
    (0..functions)
        .map(|i| {
            let extra = if i == functions / 2 { variant } else { 0 };
            format!(
                "export function handler{i}(input: Request): Response {{ return respond(input, {}); }}\n",
                i + extra
            )
        })
        .collect()
}

fn symbols(c: &mut Criterion) {
    const FUNCTIONS: usize = 2000;
    let (old, new) = (module(FUNCTIONS, 0), module(FUNCTIONS, 1));
    let path = Path::new("src/module.ts");

    let mut group = c.benchmark_group("symbols");
    group.sample_size(20);
    group.throughput(Throughput::Bytes(old.len() as u64));
    group.bench_function("parse_2000_functions", |b| {
        b.iter(|| black_box(parse(Dialect::TypeScript, old.as_bytes()).unwrap()));
    });

    // The next version of the same file, one line changed, parsed from the
    // tree of the previous one.
    let previous = parse_reusing(Dialect::TypeScript, old.as_bytes(), None).unwrap();
    group.bench_function("reparse_2000_functions_one_changed", |b| {
        b.iter(|| black_box(parse_reusing(Dialect::TypeScript, new.as_bytes(), Some(&previous)).unwrap()));
    });

    // The table as it is after the first version was recorded.
    let (parsed_old, parsed_new) = (
        parse(Dialect::TypeScript, old.as_bytes()).unwrap(),
        parse(Dialect::TypeScript, new.as_bytes()).unwrap(),
    );
    let mut table = SymbolTable::new();
    let first =
        FileChange { path, old: None, new: Some(Version { content: old.as_bytes(), parsed: &parsed_old }) };
    for op in diff_symbols(&table, LaneId::MAIN, &[first]).ops {
        table.apply(&op).unwrap();
    }
    assert_eq!(table.len(), FUNCTIONS);

    group.bench_function("diff_one_function_changed_of_2000", |b| {
        b.iter(|| {
            let change = FileChange {
                path,
                old: Some(Version { content: old.as_bytes(), parsed: &parsed_old }),
                new: Some(Version { content: new.as_bytes(), parsed: &parsed_new }),
            };
            let result = diff_symbols(&table, LaneId::MAIN, &[change]);
            assert_eq!(result.ops.len(), 1);
            black_box(result)
        });
    });
    group.finish();
}

criterion_group!(benches, symbols);
criterion_main!(benches);
