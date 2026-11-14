//! Measures the creation of a working copy for a repository of many files.
//!
//! ```text
//! cargo run --release -p kito-lane --example lane_creation -- --files 50000 --dir /path/on/the/disk/to/test
//! ```
//!
//! The target of the plan is under one second for 50,000 files where the
//! file system clones files (reflink). The example says which case it ran in.

use kito_core::{BlobId, FileMode, OpKind, Tree, TreeEntry};
use kito_lane::{materialize, remove, Source};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let files: usize = value("--files").and_then(|v| v.parse().ok()).unwrap_or(50_000);
    let dir = value("--dir").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let root = dir.join(format!("kito-lane-creation-{}", std::process::id()));
    let _ = remove(&root);
    // Whatever happens, the files do not stay behind.
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = remove(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());

    // A repository shaped like a real one: 100 files per directory, 2 kB each.
    let mut tree = Tree::new();
    let mut blobs: HashMap<BlobId, Vec<u8>> = HashMap::new();
    for i in 0..files {
        let content =
            format!("// file {i}\n{}", "export const value = compute(input) + offset;\n".repeat(44))
                .into_bytes();
        let blob = BlobId::of(&content);
        let path = PathBuf::from(format!("src/pkg{:03}/mod{:02}/file{i}.ts", i / 10_000, (i / 100) % 100));
        tree.apply(&OpKind::FileAdd { path, blob, mode: FileMode::Regular }).unwrap();
        blobs.insert(blob, content);
    }
    let read = |id: &BlobId| Ok(blobs[id].clone());

    let main = root.join("main");
    let started = Instant::now();
    materialize(&tree, &main, None, &read).unwrap();
    println!(
        "{files} files of 2 kB written from memory in {:.2?} (the main working directory)",
        started.elapsed()
    );

    let holds = |_: &Path, _: &TreeEntry| true;
    let source = Source { root: &main, holds: &holds };
    let mut best = None;
    for round in 1..=3 {
        let lane = root.join(format!("lane{round}"));
        let started = Instant::now();
        let done = materialize(&tree, &lane, Some(&source), &read).unwrap();
        let took = started.elapsed();
        remove(&lane).unwrap();
        println!(
            "lane {round}: {:.2?}  ({} cloned without copying, {} copied, {} written from the store)",
            took, done.cloned, done.copied, done.written
        );
        best = Some(best.map_or(took, |b: std::time::Duration| b.min(took)));
    }
    println!("best: {:.2?} for {files} files", best.unwrap());
}
