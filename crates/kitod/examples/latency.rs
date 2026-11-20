//! Measures the latency from a save to the op in the log, with the real daemon.
//!
//! ```text
//! cargo build --release -p kitod
//! cargo run --release -p kitod --example latency -- --saves 300 --lines 2000 [--shape dense]
//! ```
//!
//! On each round it changes one line of a file of `--lines` lines, saves it
//! and waits until the daemon has recorded the op, querying it over the
//! socket. It prints the latency seen from outside and the one measured by
//! the daemon itself, with and without the debounce.

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
        // target/<profile>/examples/latency → target/<profile>/kitod
        let exe = std::env::current_exe()?;
        let candidate: PathBuf = exe
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join(format!("kitod{}", std::env::consts::EXE_SUFFIX)))
            .unwrap_or_default();
        if !candidate.is_file() {
            return Err("kitod not found: run `cargo build --release -p kitod` first or set KITOD_BIN".into());
        }
        std::env::set_var("KITOD_BIN", candidate);
    }

    let dir = std::env::temp_dir().join(format!("kito-latency-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src"))?;
    Repo::init(&dir)?.close()?;
    let file = dir.join("src/module.ts");
    // `typical`: functions of a dozen lines, as in real code. `dense`: one
    // function per line, the worst case for symbol extraction.
    let dense =
        args.iter().position(|a| a == "--shape").and_then(|i| args.get(i + 1)).is_some_and(|v| v == "dense");
    let mut content: Vec<String> = Vec::with_capacity(lines);
    // Lines that a save may change, and how to write them with a new value.
    let mut editable: Vec<usize> = Vec::new();
    let line_with = |k: usize, value: usize| {
        if dense {
            format!(
                "export function handler{k}(input: Request): Response {{ return respond(input, {value}); }}"
            )
        } else {
            format!("  const record = store.find(input.path, {value});")
        }
    };
    let mut k = 0;
    while content.len() < lines {
        if dense {
            editable.push(content.len());
            content.push(line_with(k, k));
        } else {
            content
                .push(format!("export function handler{k}(input: Request, options: Options): Response {{"));
            content.push("  const started = clock.now();".to_owned());
            content.push("  const user = sessions.lookup(input.token);".to_owned());
            content.push("  if (!user) {".to_owned());
            content.push("    return respond(input, 401);".to_owned());
            content.push("  }".to_owned());
            editable.push(content.len());
            content.push(line_with(k, k));
            content.push("  if (record === undefined) {".to_owned());
            content.push("    return respond(input, 404);".to_owned());
            content.push("  }".to_owned());
            content.push(format!("  metrics.observe(\"handler{k}\", clock.now() - started);"));
            content.push("  return respond(input, 200, record);".to_owned());
            content.push("}".to_owned());
            content.push(String::new());
        }
        k += 1;
    }
    let functions = k;
    std::fs::write(&file, content.join("\n") + "\n")?;

    let mut client = Client::connect_or_start(&dir)?;
    let _: Value = client.call("sync", json!({}))?;
    let mut ops = client.call::<DaemonStatus>("status", json!({}))?.repo.ops;

    let mut seen_outside = Vec::with_capacity(saves);
    for i in 0..saves {
        let which = (i * 7919) % editable.len();
        content[editable[which]] = line_with(which, i + lines);
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
                return Err("the daemon did not record the save within 10 s".into());
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        seen_outside.push(start.elapsed().as_secs_f64() * 1000.0);
        // Each save is a batch of its own.
        std::thread::sleep(Duration::from_millis(30));
    }

    let status: DaemonStatus = client.call("status", json!({}))?;
    let _: Value = client.call("shutdown", json!({}))?;
    seen_outside.sort_by(|a, b| a.total_cmp(b));
    let w = &status.watcher;
    let size = std::fs::metadata(&file)?.len();
    println!(
        "file of {} lines, {functions} functions ({size} bytes), {saves} saves, debounce {} ms",
        content.len(),
        status.debounce_ms
    );
    println!();
    println!("| Measurement | p50 | p95 | max |");
    println!("| --- | --- | --- | --- |");
    println!(
        "| Save → confirmed op, as seen by the client | {:.1} ms | {:.1} ms | {:.1} ms |",
        percentile(&seen_outside, 0.50),
        percentile(&seen_outside, 0.95),
        seen_outside.last().copied().unwrap_or(0.0)
    );
    println!(
        "| First event → confirmed op, as measured by the daemon | {:.1} ms | {:.1} ms | |",
        w.latency_p50_ms.unwrap_or(0.0),
        w.latency_p95_ms.unwrap_or(0.0)
    );
    println!(
        "| Processing only, after the debounce | {:.1} ms | {:.1} ms | |",
        w.capture_p50_ms.unwrap_or(0.0),
        w.capture_p95_ms.unwrap_or(0.0)
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
