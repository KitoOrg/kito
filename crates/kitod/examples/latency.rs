//! Misura della latenza salvataggio → op nel log, con il daemon vero.
//!
//! ```text
//! cargo build --release -p kitod
//! cargo run --release -p kitod --example latency -- --saves 300 --lines 2000
//! ```
//!
//! A ogni giro cambia una riga di un file di `--lines` righe, lo salva e
//! aspetta che il daemon abbia registrato l'op, interrogandolo sul socket.
//! Stampa la latenza vista da fuori e quella misurata dal daemon stesso,
//! con e senza il debounce.

use kitod::client::Client;
use kitod::rpc::DaemonStatus;
use kitod::Repo;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let rank = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str, default: usize| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let (saves, lines) = (flag("--saves", 300), flag("--lines", 2000));

    if std::env::var_os("KITOD_BIN").is_none() {
        // target/<profilo>/examples/latency → target/<profilo>/kitod
        let exe = std::env::current_exe()?;
        let candidate: PathBuf =
            exe.parent().and_then(|p| p.parent()).map(|p| p.join("kitod")).unwrap_or_default();
        if !candidate.is_file() {
            return Err(
                "kitod non trovato: esegui prima `cargo build --release -p kitod` o imposta KITOD_BIN".into(),
            );
        }
        std::env::set_var("KITOD_BIN", candidate);
    }

    let dir = std::env::temp_dir().join(format!("kito-latency-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src"))?;
    Repo::init(&dir)?.close()?;
    let file = dir.join("src/module.ts");
    let mut content: Vec<String> = (0..lines)
        .map(|i| {
            format!("export function handler{i}(input: Request): Response {{ return respond(input, {i}); }}")
        })
        .collect();
    std::fs::write(&file, content.join("\n") + "\n")?;

    let mut client = Client::connect_or_start(&dir)?;
    let _: Value = client.call("sync", json!({}))?;
    let mut ops = client.call::<DaemonStatus>("status", json!({}))?.repo.ops;

    let mut seen_outside = Vec::with_capacity(saves);
    for i in 0..saves {
        let line = (i * 7919) % lines;
        content[line] = format!(
            "export function handler{line}(input: Request): Response {{ return respond(input, {}); }}",
            i + lines
        );
        let text = content.join("\n") + "\n";
        let start = Instant::now();
        std::fs::write(&file, text)?;
        loop {
            let now = client.call::<DaemonStatus>("status", json!({}))?.repo.ops;
            if now > ops {
                ops = now;
                break;
            }
            if start.elapsed() > Duration::from_secs(10) {
                return Err("il daemon non ha registrato il salvataggio entro 10 s".into());
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        seen_outside.push(start.elapsed().as_secs_f64() * 1000.0);
        // Ogni salvataggio è un gruppo a sé.
        std::thread::sleep(Duration::from_millis(30));
    }

    let status: DaemonStatus = client.call("status", json!({}))?;
    let _: Value = client.call("shutdown", json!({}))?;
    seen_outside.sort_by(|a, b| a.total_cmp(b));
    let w = &status.watcher;
    let size = std::fs::metadata(&file)?.len();
    println!("file di {lines} righe ({size} byte), {saves} salvataggi, debounce {} ms", status.debounce_ms);
    println!();
    println!("| Misura | p50 | p95 | massimo |");
    println!("| --- | --- | --- | --- |");
    println!(
        "| Salvataggio → op confermata, vista dal client | {:.1} ms | {:.1} ms | {:.1} ms |",
        percentile(&seen_outside, 0.50),
        percentile(&seen_outside, 0.95),
        seen_outside.last().copied().unwrap_or(0.0)
    );
    println!(
        "| Primo evento → op confermata, misurata dal daemon | {:.1} ms | {:.1} ms | |",
        w.latency_p50_ms.unwrap_or(0.0),
        w.latency_p95_ms.unwrap_or(0.0)
    );
    println!(
        "| Sola elaborazione, dopo il debounce | {:.1} ms | {:.1} ms | |",
        w.capture_p50_ms.unwrap_or(0.0),
        w.capture_p95_ms.unwrap_or(0.0)
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
