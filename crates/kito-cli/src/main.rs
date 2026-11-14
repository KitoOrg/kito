//! `kito`: the command line for humans.
//!
//! It is a thin client of the daemon (`kitod`), which it starts by itself if
//! one is not already running. Only `init` and `import` work directly on the
//! store, because they are needed before a daemon makes sense.

use clap::{Parser, Subcommand};
use kito_integrate::{ConflictInfo, IntegrationResult, JobState, Outcome as Integration};
use kitod::client::{find_root, Client, ClientError};
use kitod::identity::{self, Identity};
use kitod::lanes::LaneInfo;
use kitod::rename::RenameResult;
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
        /// Only what concerns this file; or a lane (lane/1) for its own log
        target: Option<PathBuf>,
        /// How many changes to show (what each meant for the symbols comes with it)
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        /// The log of this lane instead of the main one
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
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
        /// As this lane knows the symbol (default: the main lane)
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
    },
    /// Wait until every save has been recorded
    Sync {
        /// Only the saves of this lane
        lane: Option<String>,
    },
    /// List the lanes with agent, base, number of ops, state
    Lanes {
        /// Closed lanes too
        #[arg(long)]
        all: bool,
    },
    /// Open, close or inspect a lane
    Lane {
        #[command(subcommand)]
        command: LaneCmd,
    },
    /// Integrate one or more lanes into the main lane
    Integrate {
        /// Lanes to integrate, in this order (lane/1 lane/2 …)
        #[arg(required = true)]
        lanes: Vec<String>,
    },
    /// List the open conflicts and who they are assigned to
    Conflicts {
        /// Show this conflict in full: the three texts
        id: Option<String>,
        /// Settled conflicts too
        #[arg(long)]
        all: bool,
    },
    /// Decide a conflict: the next integration of its lane applies the decision
    Resolve {
        /// The conflict (conflict#7)
        conflict: String,
        /// Keep what the main lane has
        #[arg(long, conflicts_with_all = ["lane", "text"])]
        main: bool,
        /// Keep what the lane has
        #[arg(long, conflicts_with = "text")]
        lane: bool,
        /// Use the text in this file (`-`: standard input)
        #[arg(long, value_name = "FILE")]
        text: Option<PathBuf>,
        /// For the uses of a renamed symbol: they take the new name
        #[arg(long, conflicts_with_all = ["main", "lane", "text", "keep"])]
        rewrite: bool,
        /// For the uses of a renamed symbol: they are something else, and stay as they are
        #[arg(long, conflicts_with_all = ["main", "lane", "text"])]
        keep: bool,
    },
    /// Rename a symbol explicitly, in a lane's working copy or in the main directory
    Rename {
        /// <file>:<symbol>, or the symbol's ID
        symbol: String,
        new_name: String,
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
    },
    /// Declare the "why" of the changes recorded from now on
    Intent {
        /// What is being done, in a sentence
        summary: String,
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
        /// The task it belongs to (task #55)
        #[arg(long)]
        task: Option<String>,
    },
    /// Repository status
    Status,
    /// Manage the daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum LaneCmd {
    /// Open a lane for an agent: a working copy of its own
    Open {
        /// Name of the agent that will work in it
        agent: String,
        /// What the lane is for
        #[arg(long)]
        task: Option<String>,
        /// Checkpoint to start from (default: the main lane as it is now)
        #[arg(long, value_name = "CHECKPOINT")]
        base: Option<String>,
    },
    /// Record the last changes of a lane and remove its working copy
    Close { lane: String },
    /// Ops since the base, files touched, open conflicts
    Status { lane: String },
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

fn cmd_why(cli: &Cli, target: &str, lane: &Option<String>) -> Outcome {
    let root = root_of(cli)?;
    // The file comes first; a symbol may itself contain colons (`Foo::bar`).
    let Some((file, symbol)) = target.split_once(':').filter(|(f, s)| !f.is_empty() && !s.is_empty()) else {
        return Err("say which symbol: <file>:<symbol>, for example src/auth.ts:getUser".to_owned());
    };
    // In a lane, files are named as the lane's working copy has them.
    let file = match lane {
        Some(_) => PathBuf::from(file),
        None => repo_relative(&root, Path::new(file))?,
    };
    let mut client = daemon(cli)?;
    client.call_value("sync", json!({})).map_err(fail)?;
    let mut params = json!({ "file": file, "symbol": symbol });
    if let Some(lane) = lane {
        params["lane"] = json!(lane);
    }
    let histories: Vec<Why> = client.call("why", params).map_err(fail)?;
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

fn cmd_log(cli: &Cli, target: &Option<PathBuf>, limit: usize, lane: &Option<String>) -> Outcome {
    let root = root_of(cli)?;
    // `kito log lane/1` is the log of that lane.
    let named =
        target.as_ref().and_then(|t| t.to_str()).filter(|t| t.starts_with("lane/")).map(str::to_owned);
    let (lane, path) = match (lane, named) {
        (None, Some(named)) => (Some(named), None),
        (lane, _) => (lane.clone(), target.clone()),
    };
    let rel = match (&lane, path) {
        (_, None) => None,
        // In a lane, files are named as its working copy has them.
        (Some(_), Some(path)) => Some(path),
        (None, Some(path)) => Some(repo_relative(&root, &path)?),
    };
    let mut client = daemon(cli)?;
    // Before showing the log, make sure it contains everything that is on
    // disk, even if the watcher has missed some events.
    client.call_value("sync", json!({})).map_err(fail)?;
    let mut params = json!({ "limit": limit, "path": rel });
    if let Some(lane) = &lane {
        params["lane"] = json!(lane);
    }
    let items: Vec<LogItem> = client.call("log", params).map_err(fail)?;
    if cli.json {
        print_json(&items);
        return Ok(());
    }
    if items.is_empty() {
        println!("No operations recorded.");
    }
    // Sub-lines waiting for the change they go under, each with the file
    // of its symbol: shown only when it is not the file of that change.
    let mut pending: Vec<(String, Option<String>)> = Vec::new();
    // Symbolic ops recorded with no file change (the first analysis of files
    // that were already there) get a line of their own above them.
    let mut alone: Option<String> = None;
    let flush =
        |pending: &mut Vec<(String, Option<String>)>, alone: &mut Option<String>, under: Option<&str>| {
            if let Some(header) = alone.take() {
                println!("{header}");
            }
            for (mut line, file) in pending.drain(..).rev() {
                if let Some(file) = file.filter(|f| Some(f.as_str()) != under && !line.contains(f.as_str())) {
                    line.push_str(&format!("  ({file})"));
                }
                println!("{line}");
            }
        };
    for item in items {
        match item {
            LogItem::Checkpoint { id, label, git_commit, .. } => {
                let git = git_commit.map(|c| format!("  (git {})", &c[..10])).unwrap_or_default();
                flush(&mut pending, &mut alone, None);
                println!("◆ {id}  {label}{git}");
            }
            LogItem::Op {
                id,
                at_ms,
                author,
                op,
                path,
                detail,
                lines_added,
                lines_removed,
                intent,
                change,
                ..
            } => {
                // The first digits of the ID are enough to point `kito rollback` at the op.
                let short = &id[..id.len().min(8)];
                // What a change meant for the symbols is shown under the
                // file change it belongs to, which (going back in time)
                // comes next.
                if is_symbolic(&op) {
                    if change.is_none() {
                        // Going back in time the oldest one comes last: the header is its.
                        let mut header = format!("  {short}  {}  {author:<10} Symbols", utc(at_ms));
                        if let Some(intent) = &intent {
                            header.push_str(&format!("    [{intent}]"));
                        }
                        alone = Some(header);
                    } else if alone.is_some() {
                        flush(&mut pending, &mut alone, None);
                    }
                    pending.push((format!("      ↳ {}", detail.unwrap_or(op)), path));
                    continue;
                }
                if alone.is_some() {
                    flush(&mut pending, &mut alone, None);
                }
                let path = path.unwrap_or_default();
                let mut line = format!("  {short}  {}  {author:<10} {op:<10} {path}", utc(at_ms));
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
                flush(&mut pending, &mut alone, Some(&path));
            }
        }
    }
    flush(&mut pending, &mut alone, None);
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

fn lane_line(lane: &LaneInfo) -> String {
    let mut line = format!(
        "{:<8} {:<12} {:<11} base {:<8} {:>7}  {:>8}",
        lane.id,
        lane.agent,
        lane.state,
        lane.base,
        count(lane.ops, "op", "ops"),
        count(lane.files.len() as u64, "file", "files")
    );
    if !lane.conflicts.is_empty() {
        line.push_str(&format!("  {}", count(lane.conflicts.len() as u64, "conflict", "conflicts")));
    }
    if let Some(what) = lane.task.as_ref().or(lane.intent.as_ref()) {
        line.push_str(&format!("  {what}"));
    }
    line
}

fn cmd_lanes(cli: &Cli, all: bool) -> Outcome {
    let lanes: Vec<LaneInfo> = daemon(cli)?.call("lanes", json!({ "all": all })).map_err(fail)?;
    if cli.json {
        print_json(&lanes);
    } else if lanes.is_empty() {
        println!("No open lanes. Open one with `kito lane open <agent>`.");
    } else {
        lanes.iter().for_each(|lane| println!("{}", lane_line(lane)));
    }
    Ok(())
}

fn cmd_lane(cli: &Cli, command: &LaneCmd) -> Outcome {
    let mut client = daemon(cli)?;
    match command {
        LaneCmd::Open { agent, task, base } => {
            let lane: LaneInfo = client
                .call("open_lane", json!({ "agent": agent, "task": task, "base": base }))
                .map_err(fail)?;
            if cli.json {
                print_json(&lane);
            } else {
                println!("{} opened for {} on {}.", lane.id, lane.agent, lane.base);
                println!("Working copy: {}", lane.workdir.as_deref().unwrap_or(Path::new("")).display());
                println!(
                    "Every save there is recorded on the lane; `kito integrate {}` brings it in.",
                    lane.id
                );
            }
        }
        LaneCmd::Close { lane } => {
            let closed: LaneInfo = client.call("close_lane", json!({ "lane": lane })).map_err(fail)?;
            if cli.json {
                print_json(&closed);
            } else {
                // What a lane did before its last integration is on the
                // main lane; the rest is only in its log.
                match closed.ops {
                    0 => println!(
                        "{} closed: its working copy is gone. Nothing it did was left out of the main lane.",
                        closed.id
                    ),
                    ops => println!(
                        "{} closed: its working copy is gone. Its {} not integrated stay in the log.",
                        closed.id,
                        count(ops, "op", "ops")
                    ),
                }
            }
        }
        LaneCmd::Status { lane } => {
            let status: Value = client.call("lane_status", json!({ "lane": lane })).map_err(fail)?;
            if cli.json {
                print_json(&status);
                return Ok(());
            }
            let text = |key: &str| status[key].as_str().unwrap_or("-").to_owned();
            println!("Lane .............. {} ({})", text("id"), text("state"));
            println!("Agent ............. {}", text("agent"));
            if status["task"].is_string() {
                println!("Task .............. {}", text("task"));
            }
            if status["intent"].is_string() {
                println!("Intent ............ {}", text("intent"));
            }
            println!("Base .............. {}", text("base"));
            println!("Working copy ...... {}", text("workdir"));
            println!("Ops since the base  {}", status["ops"].as_u64().unwrap_or(0));
            for file in status["files"].as_array().into_iter().flatten() {
                println!("  changed  {}", file.as_str().unwrap_or_default());
            }
            let conflicts: Vec<ConflictInfo> =
                serde_json::from_value(status["conflicts"].clone()).unwrap_or_default();
            conflicts.iter().for_each(|c| println!("  {}", conflict_line(c)));
        }
    }
    Ok(())
}

fn conflict_line(c: &ConflictInfo) -> String {
    let what = c.symbol.clone().or_else(|| c.file.clone()).unwrap_or_default();
    let place = match (&c.symbol, &c.file) {
        (Some(_), Some(file)) => format!("  ({file})"),
        _ => String::new(),
    };
    let state = match (c.resolved, &c.resolution) {
        (true, _) => "settled".to_owned(),
        (false, Some(choice)) => format!("decided: {choice}"),
        (false, None) => "to decide".to_owned(),
    };
    format!("{:<12} {:<8} {:<14} {what}{place}  [{state}]", c.id, c.lane, c.kind)
}

fn print_integration(result: &IntegrationResult) {
    let verified = result
        .verification
        .as_ref()
        .filter(|v| v.passed && !v.commands.is_empty())
        .map(|v| format!(", verified in {:.1} s", v.duration_ms as f64 / 1000.0))
        .unwrap_or_default();
    match result.result {
        Integration::Integrated => {
            let git = result
                .git_commit
                .as_ref()
                .map(|c| format!(" (git {})", &c[..10.min(c.len())]))
                .unwrap_or_default();
            println!(
                "{}: integrated as {}{git}{verified}",
                result.lane,
                result.checkpoint.clone().unwrap_or_default()
            );
            for auto in &result.auto_resolutions {
                match (auto.kind.as_str(), &auto.new_name) {
                    ("rename_propagation", Some(new_name)) => println!(
                        "  rename followed: {} is now `{new_name}` also in {}",
                        auto.symbol,
                        auto.locations.join(", ")
                    ),
                    _ => println!("  merged by itself: {}", auto.symbol),
                }
            }
            if let Some(message) = &result.message {
                println!("  {message}");
            }
        }
        Integration::NothingToIntegrate => println!("{}: nothing to integrate", result.lane),
        Integration::Conflicts => {
            println!("{}: {}", result.lane, count(result.conflicts.len() as u64, "conflict", "conflicts"));
            result.conflicts.iter().for_each(|c| println!("  {}", conflict_line(c)));
            println!("  Decide each with `kito resolve <conflict> --main | --lane | --text FILE`, then integrate again.");
            if result.conflicts.iter().any(|c| c.kind == "semantic_ref") {
                println!("  For a semantic_ref (uses that may be of a renamed symbol): `kito resolve <conflict> --rewrite | --keep`.");
            }
        }
        Integration::VerificationFailed => {
            let report = result.verification.as_ref();
            println!(
                "{}: verification failed: `{}`",
                result.lane,
                report.and_then(|v| v.failed_command.clone()).unwrap_or_default()
            );
            for line in report.and_then(|v| v.output.clone()).unwrap_or_default().lines() {
                println!("  | {line}");
            }
            println!("  Nothing was integrated: fix it in the lane and integrate again.");
        }
        Integration::Failed => println!("{}: {}", result.lane, result.message.clone().unwrap_or_default()),
    }
}

fn cmd_integrate(cli: &Cli, lanes: &[String]) -> Outcome {
    let mut client = daemon(cli)?;
    let mut results = Vec::new();
    for lane in lanes {
        let asked: Value = client.call("request_integration", json!({ "lane": lane })).map_err(fail)?;
        let job = asked["job"].as_str().unwrap_or_default().to_owned();
        // The verification can take long: ask again until it is done.
        let result = loop {
            let state: JobState =
                client.call("integration_result", json!({ "job": job, "wait_ms": 2000 })).map_err(fail)?;
            if let JobState::Done { result } = state {
                break *result;
            }
        };
        if !cli.json {
            print_integration(&result);
        }
        results.push(result);
    }
    if cli.json {
        print_json(&results);
    }
    match results
        .iter()
        .all(|r| matches!(r.result, Integration::Integrated | Integration::NothingToIntegrate))
    {
        true => Ok(()),
        // The details are already printed; the exit code says it did not all go through.
        false => Err(String::new()),
    }
}

fn cmd_conflicts(cli: &Cli, id: &Option<String>, all: bool) -> Outcome {
    let wanted = id.as_ref().map(|id| match id.starts_with("conflict#") {
        true => id.clone(),
        false => format!("conflict#{id}"),
    });
    let conflicts: Vec<ConflictInfo> =
        daemon(cli)?.call("conflicts", json!({ "all": all || wanted.is_some() })).map_err(fail)?;
    let Some(wanted) = wanted else {
        if cli.json {
            print_json(&conflicts);
        } else if conflicts.is_empty() {
            println!("No open conflicts.");
        } else {
            conflicts.iter().for_each(|c| println!("{}", conflict_line(c)));
        }
        return Ok(());
    };
    let conflict = conflicts.iter().find(|c| c.id == wanted).ok_or(format!("{wanted}: no such conflict"))?;
    if cli.json {
        print_json(conflict);
        return Ok(());
    }
    println!("{}", conflict_line(conflict));
    for (title, text) in [
        ("at the common starting point", &conflict.base),
        ("on the main lane", &conflict.main),
        (&format!("on {}", conflict.lane), &conflict.lane_text),
    ] {
        println!("\n--- {title}");
        match text {
            Some(text) => print!("{text}{}", if text.ends_with('\n') { "" } else { "\n" }),
            None => println!("(does not exist)"),
        }
    }
    Ok(())
}

fn cmd_resolve(cli: &Cli, conflict: &str, choice: Option<&str>, text: &Option<PathBuf>) -> Outcome {
    let conflict = match conflict.starts_with("conflict#") {
        true => conflict.to_owned(),
        false => format!("conflict#{conflict}"),
    };
    let mut params = json!({ "conflict": conflict });
    match (choice, text) {
        (Some(choice), _) => params["choice"] = json!(choice),
        (None, Some(file)) => {
            let content = if file.as_os_str() == "-" {
                std::io::read_to_string(std::io::stdin()).map_err(fail)?
            } else {
                std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?
            };
            params["text"] = json!(content);
        }
        _ => return Err(
            "say how: --main, --lane or --text FILE (for the uses of a renamed symbol: --rewrite or --keep)"
                .to_owned(),
        ),
    }
    let done: Value = daemon(cli)?.call("resolve_conflict", params).map_err(fail)?;
    if cli.json {
        print_json(&done);
        return Ok(());
    }
    let lane = done["conflict"]["lane"].as_str().unwrap_or_default();
    match done["remaining"].as_u64().unwrap_or(0) {
        0 => println!("{conflict} decided. Nothing else to decide: `kito integrate {lane}` applies it."),
        n => println!("{conflict} decided. {} still to decide on {lane}.", count(n, "conflict", "conflicts")),
    }
    Ok(())
}

fn cmd_rename(cli: &Cli, symbol: &str, new_name: &str, lane: &Option<String>) -> Outcome {
    let mut params = json!({ "symbol": symbol, "new_name": new_name });
    if let Some(lane) = lane {
        params["lane"] = json!(lane);
    }
    let done: RenameResult = daemon(cli)?.call("rename_symbol", params).map_err(fail)?;
    if cli.json {
        print_json(&done);
        return Ok(());
    }
    println!("`{}` is now `{}` in {} (symbol {}).", done.from, done.to, done.file, done.symbol);
    if !done.references_updated.is_empty() {
        println!("Its uses took the new name:");
        done.references_updated.iter().for_each(|place| println!("  {place}"));
    }
    for note in &done.notes {
        println!("Note: {note}");
    }
    if !done.possible_references.is_empty() {
        println!("The old name is still written here (not touched: they may be uses of it):");
        done.possible_references.iter().for_each(|place| println!("  {place}"));
    }
    Ok(())
}

fn cmd_intent(cli: &Cli, summary: &str, lane: &Option<String>, task: &Option<String>) -> Outcome {
    let mut params = json!({ "summary": summary, "task": task });
    if let Some(lane) = lane {
        params["lane"] = json!(lane);
    }
    let done: Value = daemon(cli)?.call("set_intent", params).map_err(fail)?;
    if cli.json {
        print_json(&done);
    } else {
        println!(
            "From now on the changes on {} are recorded as: {summary}",
            done["lane"].as_str().unwrap_or("the lane")
        );
    }
    Ok(())
}

fn cmd_sync(cli: &Cli, lane: &Option<String>) -> Outcome {
    let params = match lane {
        Some(lane) => json!({ "lane": lane }),
        None => json!({}),
    };
    let done: Value = daemon(cli)?.call("sync", params).map_err(fail)?;
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
        Cmd::Log { target, limit, lane } => cmd_log(&cli, target, *limit, lane),
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
        Cmd::Why { target, lane } => cmd_why(&cli, target, lane),
        Cmd::Sync { lane } => cmd_sync(&cli, lane),
        Cmd::Lanes { all } => cmd_lanes(&cli, *all),
        Cmd::Lane { command } => cmd_lane(&cli, command),
        Cmd::Integrate { lanes } => cmd_integrate(&cli, lanes),
        Cmd::Conflicts { id, all } => cmd_conflicts(&cli, id, *all),
        Cmd::Resolve { conflict, main, lane, text, rewrite, keep } => {
            let choice = [(*main, "main"), (*lane, "lane"), (*rewrite, "rewrite"), (*keep, "keep")];
            cmd_resolve(&cli, conflict, choice.iter().find(|(set, _)| *set).map(|(_, word)| *word), text)
        }
        Cmd::Rename { symbol, new_name, lane } => cmd_rename(&cli, symbol, new_name, lane),
        Cmd::Intent { summary, lane, task } => cmd_intent(&cli, summary, lane, task),
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
        // An empty message: the command already said what went wrong.
        Err(message) if message.is_empty() => ExitCode::FAILURE,
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
