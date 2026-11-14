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

    let holds = |_: &Path, _: &TreeEntry, _: &std::fs::Metadata| true;
    let source = Source { root: &main, holds: &holds };
    // Lanes stay, as they do in a repository: with `--remove` each one is
    // removed before the next is created.
    let rounds: usize = value("--rounds").and_then(|v| v.parse().ok()).unwrap_or(5);
    let keep = !args.iter().any(|a| a == "--remove");
    let _ = std::process::Command::new("sync").status();
    let mut times = Vec::new();
    for round in 1..=rounds {
        let lane = root.join(format!("lane{round}"));
        // What the round before left to write is flushed first: on a busy
        // disk the measure would be of that.
        let _ = std::process::Command::new("sync").status();
        let (started, cpu) = (Instant::now(), cpu_seconds());
        let done = materialize(&tree, &lane, Some(&source), &read).unwrap();
        let (took, cpu) = (started.elapsed(), cpu_seconds() - cpu);
        if !keep {
            remove(&lane).unwrap();
        }
        println!(
            "lane {round}: {:.2?}, {cpu:.2} s of processor  ({} cloned without copying, {} copied, {} written from the store)",
            took, done.cloned, done.copied, done.written
        );
        times.push(took);
    }
    times.sort();
    println!("median: {:.2?}, best: {:.2?} for {files} files", times[times.len() / 2], times[0]);
}

/// Processor time this process used so far, all threads together.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // After the command name, which may contain spaces: utime and stime
    // are the 12th and 13th fields, in hundredths of a second.
    let fields: Vec<&str> = stat.rsplit(')').next().unwrap_or("").split_whitespace().collect();
    let ticks = |i: usize| fields.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (ticks(11) + ticks(12)) / 100.0
}
