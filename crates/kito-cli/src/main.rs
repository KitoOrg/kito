//! `kito`: the command line for humans.
//!
//! It is a thin client of the daemon (`kitod`), which it starts by itself if
//! one is not already running. Only `init` and `import` work directly on the
//! store, because they are needed before a daemon makes sense.

use clap::{Parser, Subcommand};
use kitod::client::{find_root, Client, ClientError};
use kitod::identity::{self, Identity};
use kitod::repo::{CheckpointResult, LogItem, RollbackResult};
use kitod::rpc::DaemonStatus;
use kitod::semantic::Why;
use kitod::Repo;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "kito", version, about = "Semantic version control for agent swarms")]
struct Cli {
    /// Repository root (default: search for `.kito/` from the current directory upwards).
    #[arg(long, global = true, value_name = "DIR")]
    repo: Option<PathBuf>,
    /// Print the same structured output as the MCP server.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create `.kito/` in a directory and start recording saves
    Init {
        /// Directory to initialize (default: the current one)
        path: Option<PathBuf>,
        /// Start from scratch even if the directory is a git repository
        #[arg(long)]
        fresh: bool,
    },
    /// Import the current state and the history of a git repository
    Import {
        /// Git repository to import (default: the current directory)
        path: Option<PathBuf>,
    },
    /// Show the operation log, most recent first
    Log {
        /// Only operations that touch this file
        path: Option<PathBuf>,
        /// How many operations to show
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
    },
    /// Create a checkpoint manually
    Checkpoint {
        /// Checkpoint label
        #[arg(short = 'm', long = "message", value_name = "LABEL")]
        label: String,
    },
    /// Export the checkpoints not yet exported to git
    ExportGit,
    /// Restore files to the state of a checkpoint or of an earlier operation
    Rollback {
        /// Point to return to: a checkpoint (ckpt#3) or the first digits
        /// of an operation ID, as shown by `kito log`
        to: Option<String>,
        /// Only these files or directories (default: everything)
        paths: Vec<PathBuf>,
        /// Go back to before the last N file changes (the lines of `kito log` that have an ID)
        #[arg(long, value_name = "N", conflicts_with = "to")]
        ops: Option<u64>,
        /// Show what would change, without touching anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Show or set who signs the changes (name and email)
    Identity {
        #[arg(long)]
        name: Option<String>,
        /// Used to attribute the commits exported to git
        #[arg(long)]
        email: Option<String>,
        /// The changes are made by an agent
        #[arg(long, conflicts_with = "human")]
        agent: bool,
        /// The changes are made by a person
        #[arg(long)]
        human: bool,
        /// Apply to all your repositories (personal configuration)
        #[arg(long)]
        global: bool,
    },
    /// Show the history of a symbol, across renames and moves
    Why {
        /// <file>:<symbol>, for example src/auth.ts:getUser or src/auth.ts:Session.refresh
        target: String,
    },
    /// Wait until every save has been recorded
    Sync,
    /// Repository status
    Status,
    /// Manage the daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Daemon and watcher status
    Status,
    /// Start the daemon if it is not running
    Start,
    /// Stop the daemon
    Stop,
}

type Outcome = Result<(), String>;

fn fail<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn root_of(cli: &Cli) -> Result<PathBuf, String> {
    let start = cli.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    find_root(&start).ok_or_else(|| ClientError::NoRepository.to_string())
}

fn daemon(cli: &Cli) -> Result<Client, String> {
    Client::connect_or_start(&root_of(cli)?).map_err(fail)
}

fn print_json(value: &impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// UTC date and time from Unix milliseconds (`2026-11-14 12:31:07`).
fn utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's algorithm: days since the epoch → civil date.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Stops the daemon if it is running and waits until it has closed the store.
fn stop_daemon(root: &Path) -> Result<bool, String> {
    let Ok(mut client) = Client::connect(root) else {
        return Ok(false);
    };
    let _: Value = client.call("shutdown", json!({})).map_err(fail)?;
    // The socket file disappears only after the store has been closed.
    let socket = kitod::rpc::socket_path(root);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(true)
}

fn cmd_init(cli: &Cli, path: &Option<PathBuf>, fresh: bool) -> Outcome {
    let root = path.clone().or_else(|| cli.repo.clone()).unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&root).map_err(fail)?;
    if root.join(".git").exists() && !fresh && !root.join(".kito").join("format").is_file() {
        return Err("this directory is a git repository: use `kito import` to bring its history along, \
                    or `kito init --fresh` to start from scratch"
            .to_owned());
    }
    let repo = Repo::init(&root).map_err(fail)?;
    let root = repo.root().to_owned();
    repo.close().map_err(fail)?;
    // Capture is continuous from the very start: the daemon starts with the repository.
    let mut client = Client::connect_or_start(&root).map_err(fail)?;
    if cli.json {
        print_json(&json!({ "root": root }));
    } else {
        println!("Kito repository created in {}", root.join(".kito").display());
        println!("The daemon is running: every save is recorded from now on.");
        warn_if_events_unreliable(&mut client);
    }
    Ok(())
}

fn cmd_import(cli: &Cli, path: &Option<PathBuf>) -> Outcome {
    let root = path.clone().or_else(|| cli.repo.clone()).unwrap_or_else(|| PathBuf::from("."));
    let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
    // The import works directly on the store: the daemon, if there is one, steps aside.
    stop_daemon(&root)?;
    let mut repo =
        if root.join(".kito").join("format").is_file() { Repo::open(&root) } else { Repo::init(&root) }
            .map_err(fail)?;
    let report = repo.import_git().map_err(fail)?;
    let captured = repo.scan().map_err(fail)?;
    repo.close().map_err(fail)?;
    let mut client = Client::connect_or_start(&root).map_err(fail)?;
    if cli.json {
        print_json(&json!({
            "commits": report.commits,
            "ops": report.ops,
            "checkpoints": report.checkpoints,
            "skipped": report.skipped,
            "uncommitted_ops": captured.len(),
        }));
    } else {
        println!(
            "Imported {}: {}, {}.",
            count(report.commits as u64, "commit", "commits"),
            count(report.ops as u64, "operation", "operations"),
            count(report.checkpoints as u64, "checkpoint", "checkpoints")
        );
        for line in &report.skipped {
            println!("  skipped {line}");
        }
        if !captured.is_empty() {
            println!("Captured {} not yet in git.", count(captured.len() as u64, "change", "changes"));
        }
        println!("The daemon is running: every save is recorded from now on.");
        warn_if_events_unreliable(&mut client);
    }
    Ok(())
}

fn cmd_why(cli: &Cli, target: &str) -> Outcome {
    let root = root_of(cli)?;
    // The file comes first; a symbol may itself contain colons (`Foo::bar`).
    let Some((file, symbol)) = target.split_once(':').filter(|(f, s)| !f.is_empty() && !s.is_empty()) else {
        return Err("say which symbol: <file>:<symbol>, for example src/auth.ts:getUser".to_owned());
    };
    let file = repo_relative(&root, Path::new(file))?;
    let mut client = daemon(cli)?;
    client.call_value("sync", json!({})).map_err(fail)?;
    let histories: Vec<Why> = client.call("why", json!({ "file": file, "symbol": symbol })).map_err(fail)?;
    if cli.json {
        print_json(&histories);
        return Ok(());
    }
    for (i, why) in histories.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{} {}  ({})  symbol {}", why.kind, why.name, why.file, why.id);
        for step in &why.steps {
            let mut line = format!("  {}  {:<10} {}", utc(step.at_ms), step.author, step.what);
            if let Some(intent) = &step.intent {
                line.push_str(&format!("  [{intent}]"));
            }
            println!("{line}");
        }
    }
    Ok(())
}

/// Ops that say what a change meant for a symbol, as opposed to the ones
/// that change a file.
fn is_symbolic(op: &str) -> bool {
    matches!(op, "AddSymbol" | "RemoveSymbol" | "ModifyBody" | "ModifySignature" | "Rename" | "Move")
}

fn cmd_log(cli: &Cli, path: &Option<PathBuf>, limit: usize) -> Outcome {
    let root = root_of(cli)?;
    let rel = path.as_ref().map(|p| repo_relative(&root, p)).transpose()?;
    let mut client = daemon(cli)?;
    // Before showing the log, make sure it contains everything that is on
    // disk, even if the watcher has missed some events.
    client.call_value("sync", json!({})).map_err(fail)?;
    let items: Vec<LogItem> = client.call("log", json!({ "limit": limit, "path": rel })).map_err(fail)?;
    if cli.json {
        print_json(&items);
        return Ok(());
    }
    if items.is_empty() {
        println!("No operations recorded.");
    }
    let mut pending: Vec<String> = Vec::new();
    for item in items {
        match item {
            LogItem::Checkpoint { id, label, git_commit, .. } => {
                let git = git_commit.map(|c| format!("  (git {})", &c[..10])).unwrap_or_default();
                pending.drain(..).rev().for_each(|sub| println!("{sub}"));
                println!("◆ {id}  {label}{git}");
            }
            LogItem::Op {
                id, at_ms, author, op, path, detail, lines_added, lines_removed, intent, ..
            } => {
                // What a change meant for the symbols is shown under the
                // file change it belongs to, which (going back in time)
                // comes next.
                if is_symbolic(&op) {
                    let mut line = format!("      ↳ {}", detail.unwrap_or(op));
                    if let Some(path) = path.filter(|p| !line.contains(p.as_str())) {
                        line.push_str(&format!("  ({path})"));
                    }
                    pending.push(line);
                    continue;
                }
                // The first digits of the ID are enough to point `kito rollback` at the op.
                let short = &id[..id.len().min(8)];
                let mut line =
                    format!("  {short}  {}  {author:<10} {op:<10} {}", utc(at_ms), path.unwrap_or_default());
                if let (Some(added), Some(removed)) = (lines_added, lines_removed) {
                    line.push_str(&format!("  +{added} −{removed}"));
                }
                if let Some(detail) = detail {
                    line.push_str(&format!("  ({detail})"));
                }
                if let Some(intent) = intent {
                    line.push_str(&format!("  [{intent}]"));
                }
                println!("{line}");
                pending.drain(..).rev().for_each(|sub| println!("{sub}"));
            }
        }
    }
    pending.drain(..).rev().for_each(|sub| println!("{sub}"));
    Ok(())
}

/// A path given as seen from the current directory, made relative to the
/// repository root.
fn repo_relative(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let abs = std::env::current_dir().map_err(fail)?.join(path);
    // The file may no longer exist: resolve the directory that contains it.
    let abs = match abs.canonicalize() {
        Ok(resolved) => resolved,
        Err(_) => match (abs.parent().and_then(|p| p.canonicalize().ok()), abs.file_name()) {
            (Some(dir), Some(name)) => dir.join(name),
            _ => abs,
        },
    };
    Ok(abs.strip_prefix(root).map(Path::to_owned).unwrap_or_else(|_| path.to_owned()))
}

fn cmd_rollback(
    cli: &Cli,
    to: &Option<String>,
    paths: &[PathBuf],
    ops: Option<u64>,
    dry_run: bool,
) -> Outcome {
    let root = root_of(cli)?;
    let to = match (to, ops) {
        (Some(to), _) => to.clone(),
        (None, Some(n)) => format!("~{n}"),
        (None, None) => {
            return Err("specify the point to return to: a checkpoint (ckpt#3), the first digits of an \
                        operation ID (see `kito log`), or --ops N"
                .to_owned())
        }
    };
    let paths = paths.iter().map(|p| repo_relative(&root, p)).collect::<Result<Vec<_>, _>>()?;
    let done: RollbackResult = daemon(cli)?
        .call("rollback", json!({ "to": to, "paths": paths, "dry_run": dry_run }))
        .map_err(fail)?;
    if cli.json {
        print_json(&done);
        return Ok(());
    }
    if done.changes.is_empty() {
        println!("The files are already in the state of {}: nothing to do.", done.target);
        return Ok(());
    }
    let files = match done.changes.len() {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    };
    if done.applied {
        println!("Rollback to {}: {files}", done.target);
    } else {
        println!("Rollback to {} would change {files} (dry run, nothing was touched):", done.target);
    }
    for change in &done.changes {
        println!("  {:<10} {}", change.action, change.path);
    }
    if let (true, Some(undo)) = (done.applied, &done.undo) {
        println!(
            "Recorded as {}: the earlier history is left intact.",
            count(done.ops as u64, "operation", "operations")
        );
        println!("To undo it: kito rollback {undo}");
    }
    Ok(())
}

fn print_identity(identity: &Identity) {
    let kind = if identity.kind == "agent" { "agent" } else { "person" };
    println!("{} ({kind})", identity.author());
    println!("Source: {}", identity.source.describe());
    if identity.email.is_none() {
        println!("No email: commits exported to git will use a placeholder address.");
        println!("To set one: kito identity --email you@example.com");
    }
}

fn cmd_identity(
    cli: &Cli,
    name: &Option<String>,
    email: &Option<String>,
    kind: Option<&str>,
    global: bool,
) -> Outcome {
    let root = root_of(cli).ok();
    let changing = name.is_some() || email.is_some() || kind.is_some();
    if changing {
        let path = if global {
            identity::personal_config_path().ok_or("cannot find the personal configuration directory")?
        } else {
            match &root {
                Some(root) => root.join(".kito").join("config.toml"),
                None => return Err(format!("{} (or use --global)", ClientError::NoRepository)),
            }
        };
        identity::write(&path, name.as_deref(), email.as_deref(), kind)?;
    }
    let identity: Identity = match &root {
        // The daemon rereads the configuration: the identity applies from the next op.
        Some(root) => {
            Client::connect_or_start(root).map_err(fail)?.call("reload", json!({})).map_err(fail)?
        }
        None => {
            let here = std::env::current_dir().map_err(fail)?;
            identity::resolve(&kitod::config::Config::default(), &here)
        }
    };
    if cli.json {
        print_json(&identity);
    } else {
        print_identity(&identity);
    }
    Ok(())
}

fn cmd_checkpoint(cli: &Cli, label: &str) -> Outcome {
    let done: CheckpointResult = daemon(cli)?.call("checkpoint", json!({ "label": label })).map_err(fail)?;
    if cli.json {
        print_json(&done);
        return Ok(());
    }
    match (&done.git_commit, &done.git_error) {
        (Some(commit), _) => println!("{} created and exported as git commit {}", done.id, &commit[..10]),
        (None, Some(error)) => println!("{} created; export to git failed: {error}", done.id),
        (None, None) => println!("{} created", done.id),
    }
    Ok(())
}

fn cmd_export(cli: &Cli) -> Outcome {
    let done: Vec<Value> = daemon(cli)?.call("export_git", json!({})).map_err(fail)?;
    if cli.json {
        print_json(&done);
    } else if done.is_empty() {
        println!("Nothing to export.");
    } else {
        for item in done {
            let commit = item["commit"].as_str().unwrap_or_default();
            println!(
                "{} → git commit {}",
                item["checkpoint"].as_str().unwrap_or_default(),
                &commit[..commit.len().min(10)]
            );
        }
    }
    Ok(())
}

fn cmd_sync(cli: &Cli) -> Outcome {
    let done: Value = daemon(cli)?.call("sync", json!({})).map_err(fail)?;
    if cli.json {
        print_json(&done);
    } else {
        println!(
            "Everything is recorded ({}).",
            count(done["ops"].as_u64().unwrap_or(0), "new operation", "new operations")
        );
    }
    Ok(())
}

fn print_status(status: &DaemonStatus, with_daemon: bool) {
    let repo = &status.repo;
    println!("Repository ........ {}", repo.root.display());
    println!("Author ............ {}", repo.author);
    println!("Tracked files ..... {}", repo.files);
    println!("Symbols ........... {}", repo.symbols);
    println!("Operations ........ {} ({} since the last checkpoint)", repo.ops, repo.ops_since_checkpoint);
    println!(
        "Checkpoints ....... {}{}",
        repo.checkpoints,
        match (repo.last_checkpoint.as_deref(), repo.unexported) {
            (Some(last), 0) => format!(" (last {last}, all exported to git)"),
            (Some(last), n) => format!(" (last {last}, {n} not yet exported to git)"),
            (None, _) => String::new(),
        }
    );
    if with_daemon {
        let w = &status.watcher;
        let ms = |v: Option<f64>| v.map_or("—".to_owned(), |v| format!("{v:.1} ms"));
        println!(
            "Daemon ............ pid {}, up for {} s, version {}",
            status.pid, status.uptime_secs, status.version
        );
        println!("Socket ............ {}", status.socket.display());
        println!(
            "Watcher ........... {}, {}, {}",
            count(w.events, "event", "events"),
            count(w.batches, "batch", "batches"),
            count(w.rescans, "full scan", "full scans")
        );
        println!(
            "Capture ........... p50 {}, p95 {} (after the {} ms debounce)",
            ms(w.capture_p50_ms),
            ms(w.capture_p95_ms),
            status.debounce_ms
        );
        println!(
            "Save → op ......... p50 {}, p95 {} (debounce included)",
            ms(w.latency_p50_ms),
            ms(w.latency_p95_ms)
        );
        if let Some(error) = &w.last_error {
            println!("Last warning ...... {error}");
        }
        println!("Reconciliation .... full scan every {}", seconds(w.rescan_interval_ms));
        if let Some(why) = unreliable_events(status) {
            println!("                    {why}");
        }
        println!("Language servers .. none (coming in phase 4)");
    }
}

fn seconds(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// If file events are not enough in this repository, explains why and what
/// Kito does instead.
fn unreliable_events(status: &DaemonStatus) -> Option<String> {
    let w = &status.watcher;
    if !w.events_unreliable {
        return None;
    }
    let cause = if !w.watching {
        "the watcher is disabled in .kito/config.toml".to_owned()
    } else if w.missed_by_watcher > 0 {
        format!("the operating system did not report {}", count(w.missed_by_watcher, "change", "changes"))
    } else {
        format!(
            "file events are unreliable on the {} file system",
            w.filesystem.as_deref().unwrap_or("underlying")
        )
    };
    Some(format!(
        "{cause}: changes are picked up by the scan, within about {}",
        seconds(w.rescan_interval_ms)
    ))
}

/// After `init` and `import`: warns if capture will not be immediate here.
fn warn_if_events_unreliable(client: &mut Client) {
    let Ok(status) = client.call::<DaemonStatus>("status", json!({})) else { return };
    if let Some(why) = unreliable_events(&status) {
        println!("Note: {why}.");
        if status.watcher.filesystem.as_deref() == Some("9p") {
            println!("      Under WSL it is better to keep the project in the Linux file system (for example in ~/projects).");
        }
    }
}

fn cmd_status(cli: &Cli, with_daemon: bool) -> Outcome {
    let mut client = daemon(cli)?;
    if !with_daemon {
        // The repository status must reflect what is on disk right now.
        client.call_value("sync", json!({})).map_err(fail)?;
    }
    let status: DaemonStatus = client.call("status", json!({})).map_err(fail)?;
    if cli.json {
        print_json(&status);
    } else {
        print_status(&status, with_daemon);
    }
    Ok(())
}

fn cmd_daemon_stop(cli: &Cli) -> Outcome {
    let stopped = stop_daemon(&root_of(cli)?)?;
    if cli.json {
        print_json(&json!({ "stopped": stopped }));
    } else if stopped {
        println!("Daemon stopped.");
    } else {
        println!("The daemon was not running.");
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match &cli.command {
        Cmd::Init { path, fresh } => cmd_init(&cli, path, *fresh),
        Cmd::Import { path } => cmd_import(&cli, path),
        Cmd::Log { path, limit } => cmd_log(&cli, path, *limit),
        Cmd::Checkpoint { label } => cmd_checkpoint(&cli, label),
        Cmd::ExportGit => cmd_export(&cli),
        Cmd::Rollback { to, paths, ops, dry_run } => cmd_rollback(&cli, to, paths, *ops, *dry_run),
        Cmd::Identity { name, email, agent, human, global } => {
            let kind = if *agent {
                Some("agent")
            } else if *human {
                Some("human")
            } else {
                None
            };
            cmd_identity(&cli, name, email, kind, *global)
        }
        Cmd::Why { target } => cmd_why(&cli, target),
        Cmd::Sync => cmd_sync(&cli),
        Cmd::Status => cmd_status(&cli, false),
        Cmd::Daemon { command: DaemonCmd::Status } => cmd_status(&cli, true),
        Cmd::Daemon { command: DaemonCmd::Start } => daemon(&cli).map(|_| {
            if !cli.json {
                println!("Daemon running.");
            }
        }),
        Cmd::Daemon { command: DaemonCmd::Stop } => cmd_daemon_stop(&cli),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            if cli.json {
                print_json(&json!({ "error": message }));
            } else {
                eprintln!("kito: {message}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::utc;

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00");
        assert_eq!(utc(1_794_659_467_000), "2026-11-14 12:31:07");
        assert_eq!(utc(4_102_444_799_999), "2099-12-31 23:59:59");
    }
}
