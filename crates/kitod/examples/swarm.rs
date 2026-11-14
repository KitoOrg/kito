//! The phase 3 demo: a swarm of simulated agents on a TypeScript project.
//!
//! ```text
//! cargo build --release -p kitod
//! cargo run --release -p kitod --example swarm -- --agents 10 --rounds 4
//! ```
//!
//! Every agent works in its own lane, all at the same time and on the same
//! files; every integration is verified (with `tsc` if it is installed)
//! before it reaches the main lane. The run fails if a single conflict is
//! found where the script has none, or if a staged real conflict is missed.
//!
//! `--verify CMD` sets the verification command, `--keep` leaves the
//! repository on disk for a look.

use kitod::client::Client;
use kitod::simulate::{project, run, Plan};
use kitod::Repo;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").current_dir(root).args(args).output().map_err(|e| e.to_string())?;
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        false => Err(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr))),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let number = |name: &str, default: usize| value(name).and_then(|v| v.parse().ok()).unwrap_or(default);
    let plan = Plan { agents: number("--agents", 10), rounds: number("--rounds", 4), real_conflicts: true };
    let keep = args.iter().any(|a| a == "--keep");

    if std::env::var_os("KITOD_BIN").is_none() {
        // target/<profile>/examples/swarm → target/<profile>/kitod
        let exe = std::env::current_exe()?;
        let candidate: PathBuf =
            exe.parent().and_then(|p| p.parent()).map(|p| p.join("kitod")).unwrap_or_default();
        if !candidate.is_file() {
            return Err("kitod not found: run `cargo build --release -p kitod` first or set KITOD_BIN".into());
        }
        std::env::set_var("KITOD_BIN", candidate);
    }
    let has_tsc = Command::new("tsc").arg("--version").output().is_ok_and(|out| out.status.success());
    let verify = value("--verify").unwrap_or_else(|| match has_tsc {
        true => "tsc --noEmit -p .".to_owned(),
        false => "! grep -rnE '^(<<<<<<<|=======|>>>>>>>)' src".to_owned(),
    });

    let root = std::env::temp_dir().join(format!("kito-swarm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    Repo::init(&root)?.close()?;
    std::fs::write(root.join(".gitignore"), ".kito/\n")?;
    for (path, content) in project(plan.agents) {
        std::fs::create_dir_all(root.join(&path).parent().ok_or("a path without a directory")?)?;
        std::fs::write(root.join(&path), content)?;
    }
    std::fs::write(
        root.join(".kito/config.toml"),
        format!(
            "[author]\nname = \"ada\"\nemail = \"ada@example.com\"\n[verify]\ncommands = [{}]\n",
            json!(verify)
        ),
    )?;
    let mut client = Client::connect_or_start(&root)?;
    let _: Value = client.call("checkpoint", json!({ "label": "the project, before the swarm" }))?;

    println!(
        "{} agents, {} rounds each, on a TypeScript project of {} files in {}",
        plan.agents,
        plan.rounds,
        project(plan.agents).len(),
        root.display()
    );
    println!("Verification before every integration: `{verify}`\n");
    let started = Instant::now();
    let report = run(&root, &plan)?;
    let took = started.elapsed();
    println!("{}", report.to_markdown());
    println!("Wall time: {:.1} s\n", took.as_secs_f64());

    // What git sees of it: one commit per integration, each by its agent.
    let log = git(&root, &["log", "--format=%an"])?;
    let mut by_author: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    log.lines().for_each(|author| *by_author.entry(author).or_default() += 1);
    println!("git log: {} commits ({})", log.lines().count(), {
        let parts: Vec<String> = by_author.iter().map(|(author, n)| format!("{author} {n}")).collect();
        parts.join(", ")
    });
    git(&root, &["fsck", "--strict", "--no-dangling"])?;
    println!(
        "git fsck --strict: ok;  git status: {}",
        match git(&root, &["status", "--porcelain"])?.trim() {
            "" => "clean",
            other => other,
        }
    );
    if has_tsc {
        let checked = Command::new("tsc").args(["--noEmit", "-p", "."]).current_dir(&root).output()?;
        println!(
            "tsc on the main lane after the swarm: {}",
            if checked.status.success() { "no errors" } else { "ERRORS" }
        );
        if !checked.status.success() {
            println!("{}", String::from_utf8_lossy(&checked.stdout));
        }
    }

    let _: Value = client.call("shutdown", json!({}))?;
    let socket = kitod::rpc::socket_path(&root);
    let deadline = Instant::now() + Duration::from_secs(30);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let verified = Repo::open(&root)?.store().verify()?;
    println!(
        "store: {} ops, {} blobs, {}",
        verified.ops,
        verified.blobs,
        match verified.is_ok() {
            true => "verified".to_owned(),
            false => format!("PROBLEMS: {:?}", verified.problems),
        }
    );
    if keep {
        println!("The repository is in {}", root.display());
    } else {
        let _ = std::fs::remove_dir_all(&root);
    }
    if !report.is_clean()
        || !verified.is_ok()
        || report.real_conflicts_found.len() != report.real_conflicts_staged
    {
        return Err("the swarm did not go as scripted".into());
    }
    println!("\nNo fake conflicts.");
    Ok(())
}
