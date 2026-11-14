//! Demo della fase 0: scrivere N op (default 1 milione), fare `kill -9` in
//! momenti casuali, riavviare e verificare che nessuna op confermata sia persa.
//!
//! ```text
//! cargo run --release -p kito-store --example crash_demo -- driver --dir /tmp/kito-demo --ops 1000000
//! ```
//!
//! Il `driver` lancia questo stesso eseguibile in modalità `writer`, che
//! scrive op deterministiche (l'op i-esima dipende solo da i e dalle op
//! precedenti) e stampa `C <n>` ogni volta che un commit ha confermato le
//! prime n. Il driver lo uccide con SIGKILL in un momento casuale, riapre lo
//! store e controlla che contenga un prefisso della sequenza lungo almeno
//! quanto l'ultimo `C <n>` letto. Poi ricomincia, fino a N op.
//!
//! I kill sono di tre tipi, scelti a caso a ogni round:
//!
//! * **dopo un commit**: SIGKILL dal driver, un tempo casuale dopo il primo
//!   commit del round;
//! * **all'avvio**: SIGKILL dal driver entro 150 ms dall'avvio, cioè anche
//!   mentre il writer sta ancora facendo il recupero dal kill precedente;
//! * **a metà scrittura**: il writer riceve SIGKILL dopo aver scritto solo una
//!   parte di un buffer, a un byte casuale. Un kill dall'esterno cade quasi
//!   sempre tra una `write` e l'altra; questo è il modo di colpire con
//!   certezza il caso peggiore, un record scritto a metà.

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

/// Generatore deterministico della sequenza di op.
struct Gen {
    n: u64,
    heads: [Option<OpId>; LANES as usize],
}

impl Gen {
    fn new() -> Gen {
        Gen { n: 0, heads: [None; LANES as usize] }
    }

    /// Riparte dallo stato di uno store che contiene le prime `n` op.
    fn resume(store: &Store) -> Result<Gen, String> {
        let n = store.op_count().map_err(|e| e.to_string())?;
        let mut heads = [None; LANES as usize];
        for lane in 0..LANES {
            let expected = n / LANES + u64::from(lane < n % LANES);
            let found = store.lane_len(LaneId(lane)).map_err(|e| e.to_string())?;
            if found != expected {
                return Err(format!(
                    "lane {lane}: {found} op, attese {expected}: lo store non è un prefisso"
                ));
            }
            heads[lane as usize] = store.lane_head(LaneId(lane)).map_err(|e| e.to_string())?;
        }
        Ok(Gen { n, heads })
    }

    fn content(i: u64) -> Vec<u8> {
        if i % 2500 == 7 {
            // Ogni tanto un blob grande e incomprimibile.
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
                (OpKind::FileAdd { path: file, blob: BlobId::of(&c) }, vec![c])
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
            // Ogni tanto un'op con due genitori: l'op precedente, di un'altra lane.
            parents.extend(self.heads[((i - 1) % LANES) as usize]);
        }
        let author = Author::agent(format!("agent-{}", lane.0));
        let op = Op::new(parents, lane, author, Hlc::new(1_700_000_000_000 + i, 0), kind, None)
            .expect("op valida");
        self.heads[lane.0 as usize] = Some(op.id);
        (op, blobs)
    }
}

/// splitmix64: basta e avanza per scegliere quando uccidere il writer.
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

/// File system che uccide il processo con SIGKILL a metà di una scrittura,
/// dopo un certo numero di byte.
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
            // Arriva su file solo l'inizio del buffer, poi il processo muore.
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
    let mut store = Store::open_with(dir, opts).map_err(|e| format!("apertura: {e}"))?;
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
        // Solo ora le prime `gen.n` op sono confermate.
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

/// Verifica che lo store contenga le op `from..n` attese, blob compresi.
fn verify_range(store: &Store, vgen: &mut Gen, n: u64) -> Result<(), String> {
    while vgen.n < n {
        let i = vgen.n;
        let (expected, blobs) = vgen.next();
        match store.get_op(&expected.id) {
            Ok(Some(found)) if found == expected => {}
            Ok(Some(_)) => return Err(format!("op {i}: contenuto diverso da quello atteso")),
            Ok(None) => return Err(format!("op {i} ({}) assente", expected.id)),
            Err(e) => return Err(format!("op {i}: {e}")),
        }
        for blob in &blobs {
            match store.get_blob(&BlobId::of(blob)) {
                Ok(Some(found)) if found == *blob => {}
                Ok(_) => return Err(format!("op {i}: blob assente o diverso")),
                Err(e) => return Err(format!("op {i}: blob: {e}")),
            }
        }
    }
    Ok(())
}

fn driver(dir: &Path, target: u64, seed: u64, max_ms: u64) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if dir.exists() {
        return Err(format!("{} esiste già: scegli una cartella nuova", dir.display()));
    }
    drop(Store::init(dir).map_err(|e| e.to_string())?);

    let mut rng = Rng(seed | 1);
    let mut totals = Totals::default();
    let mut confirmed_max = 0u64;
    let mut vgen = Gen::new();
    let started = Instant::now();
    println!("crash demo: {target} op in {}, seed {seed}", dir.display());

    loop {
        totals.rounds += 1;
        // Tipo di kill di questo round.
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
        let mut child = command.spawn().map_err(|e| format!("avvio del writer: {e}"))?;
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
                Kill::MidWrite(_) => false, // il writer muore da solo
            };
            if due {
                child.kill().map_err(|e| e.to_string())?; // SIGKILL
                break child.wait().map_err(|e| e.to_string())?;
            }
            std::thread::sleep(Duration::from_micros(200));
        };
        let killed_at = spawned.elapsed();
        reader.join().map_err(|_| "thread di lettura fallito".to_string())?;
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
            return Err(format!("il writer è terminato da solo con {status}: guarda il suo stderr"));
        };
        let how = match (finished, &kill) {
            (true, _) => "terminato               ".to_string(),
            (_, Kill::AfterCommit(_)) => format!("kill dopo commit +{:>4} ms", killed_at.as_millis()),
            (_, Kill::AtStartup(_)) => format!("kill all'avvio  +{:>4} ms", killed_at.as_millis()),
            (_, Kill::MidWrite(_)) => format!("kill a metà write +{:>4} ms", killed_at.as_millis()),
        };

        // Non sempre si verifica subito: così a volte è il writer successivo
        // a fare il recupero, e può essere ucciso mentre lo fa.
        if finished || rng.below(10) < 6 {
            totals.verifications += 1;
            let store = Store::open(dir).map_err(|e| format!("riapertura dopo il kill: {e}"))?;
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
                return Err(format!("PERSE OP CONFERMATE: confermate {confirmed_max}, presenti {n}"));
            }
            Gen::resume(&store)?;
            verify_range(&store, &mut vgen, n)?;
            println!(
                "round {:>3}: {how} | confermate {:>8} | presenti {:>8} | scartati {:>7} B | indice {:?}",
                totals.rounds,
                confirmed_max,
                n,
                recovery.discarded_bytes(),
                recovery.index,
            );
        } else {
            println!(
                "round {:>3}: {how} | confermate {confirmed_max:>8} | verifica rimandata",
                totals.rounds
            );
        }
        if finished {
            break;
        }
    }
    let writing = started.elapsed();

    // Controllo finale: tutte le op attese, fsck completo, indice ricostruito
    // da zero identico a quello incrementale.
    let mut store = Store::open(dir).map_err(|e| e.to_string())?;
    let n = store.op_count().map_err(|e| e.to_string())?;
    if n != target {
        return Err(format!("presenti {n} op, attese {target}"));
    }
    let mut full = Gen::new();
    verify_range(&store, &mut full, n)?;
    let report = store.verify().map_err(|e| e.to_string())?;
    if !report.is_ok() {
        return Err(format!(
            "verify ha trovato problemi: {:#?}",
            &report.problems[..report.problems.len().min(10)]
        ));
    }
    let before = (store.lanes().map_err(|e| e.to_string())?, store.stats().map_err(|e| e.to_string())?);
    store.rebuild_index().map_err(|e| format!("ricostruzione dell'indice: {e}"))?;
    let after = (store.lanes().map_err(|e| e.to_string())?, store.stats().map_err(|e| e.to_string())?);
    if before != after {
        return Err("l'indice ricostruito differisce da quello incrementale".into());
    }
    let mut again = Gen::new();
    verify_range(&store, &mut again, n)?;

    println!();
    println!("op scritte e verificate ........ {n}");
    println!("blob ........................... {}", report.blobs);
    println!("segmenti oplog / pack .......... {} / {}", after.1.oplog_segments, after.1.packs);
    println!("round .......................... {}", totals.rounds);
    let kills = totals.kills_after_commit + totals.kills_at_startup + totals.kills_mid_write;
    println!("kill -9 ........................ {kills}");
    println!("  dopo un commit ............... {}", totals.kills_after_commit);
    println!("  all'avvio (anche in recupero)  {}", totals.kills_at_startup);
    println!("  a metà di una scrittura ...... {}", totals.kills_mid_write);
    println!("  prima del primo commit ....... {}", totals.kills_before_first_commit);
    println!("verifiche dopo un kill ......... {}", totals.verifications);
    println!(
        "code troncate scartate ......... {} ({} byte, mai confermati)",
        totals.torn_tails, totals.discarded_bytes
    );
    println!("indice riallineato / rifatto ... {} / {}", totals.caught_up, totals.rebuilt);
    println!("tempo di scrittura con i kill .. {:.1} s", writing.as_secs_f64());
    println!("op confermate perse ............ 0");
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
        _ => Err("uso: crash_demo driver|writer --dir <cartella> [--ops N] [--seed S] [--max-ms M]".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("crash_demo: ERRORE: {e}");
            ExitCode::FAILURE
        }
    }
}
