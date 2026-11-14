//! Phase 0 demo: write N ops (default 1 million), `kill -9` at random
//! moments, restart and verify that no confirmed op has been lost.
//!
//! ```text
//! cargo run --release -p kito-store --example crash_demo -- driver --dir /tmp/kito-demo --ops 1000000
//! ```
//!
//! The `driver` launches this same executable in `writer` mode, which writes
//! deterministic ops (the i-th op depends only on i and on the preceding
//! ops) and prints `C <n>` every time a commit has confirmed the first n.
//! The driver kills it with SIGKILL at a random moment, reopens the store
//! and checks that it holds a prefix of the sequence at least as long as the
//! last `C <n>` it read. Then it starts again, up to N ops.
//!
//! The kills are of three kinds, chosen at random on every round:
//!
//! * **after a commit**: SIGKILL from the driver, a random time after the
//!   first commit of the round;
//! * **at startup**: SIGKILL from the driver within 150 ms of startup, that
//!   is, even while the writer is still recovering from the previous kill;
//! * **mid-write**: the writer receives SIGKILL after writing only part of a
//!   buffer, at a random byte. A kill from outside almost always lands
//!   between one `write` and the next; this is the way to hit with certainty
//!   the worst case, a half-written record.

use kito_core::*;
use kito_store::fs::{FileSystem, RawFile, StdFs};
use kito_store::{IndexRecovery, Store, StoreOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const LANES: u64 = 4;

/// Deterministic generator of the sequence of ops.
struct Gen {
    n: u64,
    heads: [Option<OpId>; LANES as usize],
}

impl Gen {
    fn new() -> Gen {
        Gen { n: 0, heads: [None; LANES as usize] }
    }

    /// Resumes from the state of a store that holds the first `n` ops.
    fn resume(store: &Store) -> Result<Gen, String> {
        let n = store.op_count().map_err(|e| e.to_string())?;
        let mut heads = [None; LANES as usize];
        for lane in 0..LANES {
            let expected = n / LANES + u64::from(lane < n % LANES);
            let found = store.lane_len(LaneId(lane)).map_err(|e| e.to_string())?;
            if found != expected {
                return Err(format!(
                    "lane {lane}: {found} ops, expected {expected}: the store is not a prefix"
                ));
            }
            heads[lane as usize] = store.lane_head(LaneId(lane)).map_err(|e| e.to_string())?;
        }
        Ok(Gen { n, heads })
    }

    fn content(i: u64) -> Vec<u8> {
        if i % 2500 == 7 {
            // Every now and then a large, incompressible blob.
            let mut x = i | 1;
            return (0..100_000)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect();
        }
        let lines = 1 + (i * 7) % 20;
        (0..lines)
            .flat_map(|l| {
                format!("export function f{i}_{l}(x: number) {{ return x + {}; }}\n", i * l).into_bytes()
            })
            .collect()
    }

    fn next(&mut self) -> (Op, Vec<Vec<u8>>) {
        let i = self.n;
        self.n += 1;
        let lane = LaneId(i % LANES);
        let sym = SymbolId::new(lane, i % 5000);
        let file = PathBuf::from(format!("src/mod{}/f{}.ts", i % 50, i % 1000));
        let (kind, blobs) = match i % 6 {
            0 => {
                let c = Gen::content(i);
                (OpKind::FileAdd { path: file, blob: BlobId::of(&c), mode: FileMode::Regular }, vec![c])
            }
            1 => {
                let (a, b) = (Gen::content(i - 1), Gen::content(i));
                let edits = vec![TextEdit::new(0, 3, format!("v{i}"))];
                (OpKind::TextEdit { file, before: BlobId::of(&a), after: BlobId::of(&b), edits }, vec![a, b])
            }
            2 => {
                let (a, b) = (Gen::content(i - 2), Gen::content(i));
                let kind = OpKind::ModifyBody {
                    id: sym,
                    before: BlobId::of(&a),
                    after: BlobId::of(&b),
                    edits: vec![],
                };
                (kind, vec![a, b])
            }
            3 => (OpKind::Rename { id: sym, from: format!("f{i}"), to: format!("g{i}") }, vec![]),
            4 => (OpKind::FileRemove { path: file }, vec![]),
            _ => {
                let c = Gen::content(i);
                let symbol = Symbol {
                    id: sym,
                    kind: SymbolKind::Function,
                    lang: Lang::TypeScript,
                    name: format!("f{i}"),
                    parent: None,
                    file,
                    signature: Hash::of(&i.to_le_bytes()),
                    body: Hash::of(&c),
                    fingerprint: Fingerprint {
                        sketch: (0..16).map(|k| (i as u32).wrapping_mul(k + 1)).collect(),
                    },
                };
                (OpKind::AddSymbol { sym: symbol, text: BlobId::of(&c) }, vec![c])
            }
        };
        let mut parents: Vec<OpId> = self.heads[lane.0 as usize].into_iter().collect();
        if i > 0 && i.is_multiple_of(97) {
            // Every now and then an op with two parents: the previous op, from another lane.
            parents.extend(self.heads[((i - 1) % LANES) as usize]);
        }
        let author = Author::agent(format!("agent-{}", lane.0));
        let op =
            Op::new(parents, lane, author, Hlc::new(1_700_000_000_000 + i, 0), kind, None).expect("valid op");
        self.heads[lane.0 as usize] = Some(op.id);
        (op, blobs)
    }
}

/// splitmix64: more than enough to choose when to kill the writer.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// File system that kills the process with SIGKILL in the middle of a write,
/// after a given number of bytes.
#[derive(Debug, Clone)]
struct DyingFs {
    bytes_left: Arc<AtomicI64>,
}

struct DyingFile {
    inner: Box<dyn RawFile>,
    bytes_left: Arc<AtomicI64>,
}

impl RawFile for DyingFile {
    fn append(&mut self, buf: &[u8]) -> io::Result<()> {
        let left = self.bytes_left.fetch_sub(buf.len() as i64, Ordering::SeqCst);
        if left < buf.len() as i64 {
            // Only the beginning of the buffer reaches the file, then the process dies.
            self.inner.append(&buf[..left.max(0) as usize])?;
            let _ = Command::new("kill").args(["-9", &std::process::id().to_string()]).status();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        self.inner.append(buf)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.inner.sync()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.inner.truncate(len)
    }
}

impl FileSystem for DyingFs {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn RawFile>> {
        Ok(Box::new(DyingFile { inner: StdFs.open_append(path)?, bytes_left: self.bytes_left.clone() }))
    }

    fn create_atomic(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        StdFs.create_atomic(path, content)
    }
}

fn now_seed() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    (t.as_nanos() as u64) ^ ((std::process::id() as u64) << 32) | 1
}

fn writer(dir: &Path, target: u64, die_after_bytes: Option<i64>) -> Result<(), String> {
    let mut opts = StoreOptions::default();
    if let Some(bytes) = die_after_bytes {
        opts.fs = Arc::new(DyingFs { bytes_left: Arc::new(AtomicI64::new(bytes)) });
    }
    let mut store = Store::open_with(dir, opts).map_err(|e| format!("open: {e}"))?;
    let mut gen = Gen::resume(&store)?;
    let mut rng = Rng(now_seed());
    let stdout = std::io::stdout();
    while gen.n < target {
        let batch = (1 + rng.below(4000)).min(target - gen.n);
        for _ in 0..batch {
            let (op, blobs) = gen.next();
            for blob in &blobs {
                store.put_blob(blob).map_err(|e| format!("put_blob: {e}"))?;
            }
            store.append_op(&op).map_err(|e| format!("append_op: {e}"))?;
        }
        store.commit().map_err(|e| format!("commit: {e}"))?;
        // Only now are the first `gen.n` ops confirmed.
        let mut out = stdout.lock();
        writeln!(out, "C {}", gen.n).and_then(|()| out.flush()).map_err(|e| e.to_string())?;
    }
    store.close().map_err(|e| format!("close: {e}"))?;
    println!("DONE");
    Ok(())
}

#[derive(Default)]
struct Totals {
    rounds: u32,
    kills_after_commit: u32,
    kills_at_startup: u32,
    kills_mid_write: u32,
    kills_before_first_commit: u32,
    torn_tails: u32,
    discarded_bytes: u64,
    caught_up: u32,
    rebuilt: u32,
    verifications: u32,
}

/// Verifies that the store holds the expected ops `from..n`, blobs included.
fn verify_range(store: &Store, vgen: &mut Gen, n: u64) -> Result<(), String> {
    while vgen.n < n {
        let i = vgen.n;
        let (expected, blobs) = vgen.next();
        match store.get_op(&expected.id) {
            Ok(Some(found)) if found == expected => {}
            Ok(Some(_)) => return Err(format!("op {i}: content differs from the expected one")),
            Ok(None) => return Err(format!("op {i} ({}) missing", expected.id)),
            Err(e) => return Err(format!("op {i}: {e}")),
        }
        for blob in &blobs {
            match store.get_blob(&BlobId::of(blob)) {
                Ok(Some(found)) if found == *blob => {}
                Ok(_) => return Err(format!("op {i}: blob missing or different")),
                Err(e) => return Err(format!("op {i}: blob: {e}")),
            }
        }
    }
    Ok(())
}

fn driver(dir: &Path, target: u64, seed: u64, max_ms: u64) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if dir.exists() {
        return Err(format!("{} already exists: choose a new directory", dir.display()));
    }
    drop(Store::init(dir).map_err(|e| e.to_string())?);

    let mut rng = Rng(seed | 1);
    let mut totals = Totals::default();
    let mut confirmed_max = 0u64;
    let mut vgen = Gen::new();
    let started = Instant::now();
    println!("crash demo: {target} ops in {}, seed {seed}", dir.display());

    loop {
        totals.rounds += 1;
        // Kind of kill for this round.
        #[derive(PartialEq)]
        enum Kill {
            AfterCommit(Duration),
            AtStartup(Duration),
            MidWrite(u64),
        }
        let kill = match rng.below(10) {
            0..=3 => Kill::AfterCommit(Duration::from_micros(rng.below(max_ms * 1000))),
            4..=5 => Kill::AtStartup(Duration::from_micros(rng.below(150_000))),
            _ => Kill::MidWrite(rng.below(3_000_000)),
        };
        let mut command = Command::new(&exe);
        command
            .args(["writer", "--dir"])
            .arg(dir)
            .args(["--ops", &target.to_string()])
            .stdout(Stdio::piped());
        if let Kill::MidWrite(bytes) = kill {
            command.args(["--die-after-bytes", &bytes.to_string()]);
        }
        let mut child = command.spawn().map_err(|e| format!("starting the writer: {e}"))?;
        let confirmed = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let (confirmed, done) = (confirmed.clone(), done.clone());
            let stdout = child.stdout.take().unwrap();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(n) = line.strip_prefix("C ").and_then(|n| n.parse().ok()) {
                        confirmed.store(n, Ordering::SeqCst);
                    } else if line == "DONE" {
                        done.store(true, Ordering::SeqCst);
                    }
                }
            })
        };

        let spawned = Instant::now();
        let mut first_commit: Option<Instant> = None;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                break status;
            }
            if first_commit.is_none() && confirmed.load(Ordering::SeqCst) > 0 {
                first_commit = Some(Instant::now());
            }
            let due = match kill {
                Kill::AfterCommit(delay) => first_commit.is_some_and(|t| t.elapsed() >= delay),
                Kill::AtStartup(delay) => spawned.elapsed() >= delay,
                Kill::MidWrite(_) => false, // the writer dies on its own
            };
            if due {
                child.kill().map_err(|e| e.to_string())?; // SIGKILL
                break child.wait().map_err(|e| e.to_string())?;
            }
            std::thread::sleep(Duration::from_micros(200));
        };
        let killed_at = spawned.elapsed();
        reader.join().map_err(|_| "reader thread failed".to_string())?;
        let seen = confirmed.load(Ordering::SeqCst);
        confirmed_max = confirmed_max.max(seen);

        let finished = if status.signal() == Some(9) {
            match kill {
                Kill::AfterCommit(_) => totals.kills_after_commit += 1,
                Kill::AtStartup(_) => totals.kills_at_startup += 1,
                Kill::MidWrite(_) => totals.kills_mid_write += 1,
            }
            totals.kills_before_first_commit += u32::from(seen == 0);
            false
        } else if status.success() && done.load(Ordering::SeqCst) {
            true
        } else {
            return Err(format!("the writer exited on its own with {status}: look at its stderr"));
        };
        let how = match (finished, &kill) {
            (true, _) => "finished                  ".to_string(),
            (_, Kill::AfterCommit(_)) => format!("kill after commit +{:>4} ms", killed_at.as_millis()),
            (_, Kill::AtStartup(_)) => format!("kill at startup   +{:>4} ms", killed_at.as_millis()),
            (_, Kill::MidWrite(_)) => format!("kill mid-write    +{:>4} ms", killed_at.as_millis()),
        };

        // The check is not always done right away: that way it is sometimes
        // the next writer that runs recovery, and it can be killed while doing so.
        if finished || rng.below(10) < 6 {
            totals.verifications += 1;
            let store = Store::open(dir).map_err(|e| format!("reopening after the kill: {e}"))?;
            let recovery = store.recovery().clone();
            if recovery.discarded_bytes() > 0 {
                totals.torn_tails += 1;
                totals.discarded_bytes += recovery.discarded_bytes();
            }
            match recovery.index {
                IndexRecovery::UpToDate => {}
                IndexRecovery::CaughtUp { .. } => totals.caught_up += 1,
                IndexRecovery::Rebuilt { .. } => totals.rebuilt += 1,
            }
            let n = store.op_count().map_err(|e| e.to_string())?;
            if n < confirmed_max {
                return Err(format!("CONFIRMED OPS LOST: confirmed {confirmed_max}, present {n}"));
            }
            Gen::resume(&store)?;
            verify_range(&store, &mut vgen, n)?;
            println!(
                "round {:>3}: {how} | confirmed {:>8} | present {:>8} | discarded {:>7} B | index {:?}",
                totals.rounds,
                confirmed_max,
                n,
                recovery.discarded_bytes(),
                recovery.index,
            );
        } else {
            println!(
                "round {:>3}: {how} | confirmed {confirmed_max:>8} | verification deferred",
                totals.rounds
            );
        }
        if finished {
            break;
        }
    }
    let writing = started.elapsed();

    // Final check: all the expected ops, full fsck, index rebuilt from
    // scratch identical to the incremental one.
    let mut store = Store::open(dir).map_err(|e| e.to_string())?;
    let n = store.op_count().map_err(|e| e.to_string())?;
    if n != target {
        return Err(format!("{n} ops present, expected {target}"));
    }
    let mut full = Gen::new();
    verify_range(&store, &mut full, n)?;
    let report = store.verify().map_err(|e| e.to_string())?;
    if !report.is_ok() {
        return Err(format!(
            "verify found problems: {:#?}",
            &report.problems[..report.problems.len().min(10)]
        ));
    }
    let before = (store.lanes().map_err(|e| e.to_string())?, store.stats().map_err(|e| e.to_string())?);
    store.rebuild_index().map_err(|e| format!("rebuilding the index: {e}"))?;
    let after = (store.lanes().map_err(|e| e.to_string())?, store.stats().map_err(|e| e.to_string())?);
    if before != after {
        return Err("the rebuilt index differs from the incremental one".into());
    }
    let mut again = Gen::new();
    verify_range(&store, &mut again, n)?;

    println!();
    println!("ops written and verified ....... {n}");
    println!("blobs .......................... {}", report.blobs);
    println!("oplog segments / packs ......... {} / {}", after.1.oplog_segments, after.1.packs);
    println!("rounds ......................... {}", totals.rounds);
    let kills = totals.kills_after_commit + totals.kills_at_startup + totals.kills_mid_write;
    println!("kill -9 ........................ {kills}");
    println!("  after a commit ............... {}", totals.kills_after_commit);
    println!("  at startup (even mid-recovery) {}", totals.kills_at_startup);
    println!("  in the middle of a write ..... {}", totals.kills_mid_write);
    println!("  before the first commit ...... {}", totals.kills_before_first_commit);
    println!("verifications after a kill ..... {}", totals.verifications);
    println!(
        "torn tails discarded ........... {} ({} bytes, never confirmed)",
        totals.torn_tails, totals.discarded_bytes
    );
    println!("index caught up / rebuilt ...... {} / {}", totals.caught_up, totals.rebuilt);
    println!("write time with the kills ...... {:.1} s", writing.as_secs_f64());
    println!("confirmed ops lost ............. 0");
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let dir = flag("--dir").map(PathBuf::from);
    let ops: u64 = flag("--ops").and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let result = match (args.first().map(String::as_str), dir) {
        (Some("writer"), Some(dir)) => {
            writer(&dir, ops, flag("--die-after-bytes").and_then(|v| v.parse().ok()))
        }
        (Some("driver"), Some(dir)) => {
            let seed = flag("--seed").and_then(|v| v.parse().ok()).unwrap_or_else(now_seed);
            let max_ms = flag("--max-ms").and_then(|v| v.parse().ok()).unwrap_or(400);
            driver(&dir, ops, seed, max_ms)
        }
        _ => {
            Err("usage: crash_demo driver|writer --dir <directory> [--ops N] [--seed S] [--max-ms M]".into())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("crash_demo: ERROR: {e}");
            ExitCode::FAILURE
        }
    }
}
