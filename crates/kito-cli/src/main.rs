//! `kito`: the command line for humans.
//!
//! It is a thin client of the daemon (`kitod`), which it starts by itself if
//! one is not already running. Only `init` and `import` work directly on the
//! store, because they are needed before a daemon makes sense.

use clap::{Parser, Subcommand};
use kito_core::paths::Canonical as _;
use kito_integrate::{ConflictInfo, IntegrationResult, JobState, Outcome as Integration};
use kitod::client::{find_root, Client, ClientError, Remote};
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

mod claude;
mod hook;
mod log;

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
        /// Do not set the project up for Claude Code (see `kito claude`)
        #[arg(long)]
        no_claude: bool,
    },
    /// Import the current state and the history of a git repository
    Import {
        /// Git repository to import (default: the current directory)
        path: Option<PathBuf>,
        /// Do not set the project up for Claude Code (see `kito claude`)
        #[arg(long)]
        no_claude: bool,
    },
    /// Set the project up for Claude Code: the MCP server, the permission to
    /// call it, the rules in CLAUDE.md (`init` and `import` do it too)
    Claude,
    /// Show the operation log, most recent first
    Log {
        /// Only what concerns this file; or a lane (lane/1) for its own log
        target: Option<PathBuf>,
        /// How many lines to show (with -v: how many saves)
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        /// The log of this lane instead of the main one
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
        /// Every save on a line of its own, with what it meant for each
        /// symbol under it
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Create a checkpoint manually
    Checkpoint {
        /// Checkpoint label
        #[arg(short = 'm', long = "message", value_name = "LABEL")]
        label: String,
    },
    /// Export the checkpoints not yet exported to git
    ExportGit,
    /// Import a SCIP index of the repository: who uses what, found by an indexer in one go
    Scip {
        /// The index (index.scip), made on the files as they are now
        file: PathBuf,
    },
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
    /// Why a function, a file or some lines are the way they are: the
    /// reasons they changed, what changed with them, who uses them, who is
    /// on them now
    Why {
        /// <file>:<function> (src/auth.ts:getUser, src/auth.ts:Session.refresh),
        /// a file, or lines: <file>:<line> or <file>:<line>-<line>
        target: String,
        /// As this lane has it, its own changes included (default: the main lane)
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
        /// Every step in the life of a symbol, one by one, across renames
        /// and moves
        #[arg(long)]
        history: bool,
        /// The answer as a diagram (Mermaid), to paste where diagrams are drawn
        #[arg(long, conflicts_with = "history")]
        graph: bool,
        /// Look for who uses it also in a repository with thousands of files
        #[arg(long)]
        uses: bool,
    },
    /// Find earlier work by the words of its reason
    Reasons {
        /// Words that must all be in the reason, its task, or the names of
        /// who did it (none: the latest reasons)
        words: Vec<String>,
        /// How many at most
        #[arg(short = 'n', long, default_value_t = 20, value_name = "N")]
        limit: usize,
    },
    /// Leave a note on a function, a file or a directory: something to know
    /// about that code that no change says ("called by the nightly job: do
    /// not rename"). It is told with the why of that code
    Note {
        /// <file>:<function>, a file, a directory (src/billing/), or lines
        /// (the function they are in)
        target: Option<String>,
        /// What to know about it
        text: Vec<String>,
        /// The code as this lane has it (default: the main lane)
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
        /// List the notes, all or those on the target
        #[arg(long, conflicts_with = "remove")]
        list: bool,
        /// Remove a note (note#3): it stays in the history, no longer shown
        #[arg(long, value_name = "NOTE")]
        remove: Option<String>,
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
    /// Connect a directory to a repository kept by a Kito server
    Connect {
        /// The server: host:port, or ssh://user@host/path/of/the/server
        server: String,
        /// The repository on the server
        #[arg(value_name = "REPO")]
        name: String,
        /// The directory to connect (default: a new one named as the repository)
        dir: Option<PathBuf>,
        /// The token the server gave you, for host:port (default: $KITO_TOKEN)
        #[arg(long)]
        token: Option<String>,
        /// The fingerprint of the server's certificate, as `kito-server
        /// fingerprint` prints it there: what says the server you reach
        /// is the one you mean. Not needed for a server on this machine
        #[arg(long, value_name = "SHA256")]
        fingerprint: Option<String>,
        /// The server's certificate comes from a public authority: it is
        /// known by its name
        #[arg(long, conflicts_with = "fingerprint")]
        authority: bool,
        /// The server does not encrypt (`kito-server serve --plain`): on
        /// this machine only
        #[arg(long, conflicts_with_all = ["fingerprint", "authority"])]
        plain: bool,
        /// Do not set the directory up for Claude Code
        #[arg(long)]
        no_claude: bool,
    },
    /// The repositories you may use on the server this directory is connected to
    Repos,
    /// What happens in the repository, as it happens
    Watch {
        /// Only what happens in this lane
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
        /// Stop after this many events
        #[arg(short = 'n', long, value_name = "N")]
        count: Option<usize>,
        /// Start with what the daemon still remembers, not from now
        #[arg(long)]
        past: bool,
    },
    /// Every open lane: whose it is, what for, what it declared and
    /// changed, and which lanes are on the same code
    Board,
    /// The lanes that declared or changed a function, a file or a directory
    Who {
        /// src/auth.ts:getUser, src/auth.ts or src/billing/
        target: String,
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
    /// Bring in the commits the git remote has and the main lane does
    /// not (made with plain git by people who do not work through Kito):
    /// they are merged through a lane, and the result is pushed back.
    /// Done by itself before every push
    Pull,
    /// What Claude Code runs around its tools, written into the project by
    /// `kito claude`: edits are checked before they are made
    Hook {
        #[command(subcommand)]
        what: HookCmd,
    },
    /// Protect the main lane: from then on it changes only when a lane is
    /// integrated, and a file edited in the main directory is put back
    Protect {
        /// Let the main directory be edited by hand again
        #[arg(long)]
        off: bool,
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
        /// Why the lane is opened, in one sentence
        #[arg(long)]
        intent: String,
        /// What the lane will change: functions (src/auth.ts:getUser),
        /// files, directories (src/billing/). Once per item, or several
        /// after one --scope
        #[arg(long, required = true, num_args = 1.., value_name = "ITEM")]
        scope: Vec<String>,
        /// The task the lane belongs to (task #55)
        #[arg(long)]
        task: Option<String>,
        /// Checkpoint to start from (default: the main lane as it is now)
        #[arg(long, value_name = "CHECKPOINT", conflicts_with = "on")]
        base: Option<String>,
        /// Start on another lane as it is now, before it has landed; the
        /// new lane lands on it unless --target says otherwise
        #[arg(long, value_name = "LANE")]
        on: Option<String>,
        /// The lane to land on: the one it starts on, or one below it
        /// (main). It waits until the work it was built on is there
        #[arg(long, value_name = "LANE")]
        target: Option<String>,
    },
    /// Create a permanent lane, such as `staging`: lanes land on it under
    /// checkpoints of its own, it has its own git branch and checks, and
    /// `kito integrate <name>` lands it on main without closing it
    Create {
        /// Its name: lowercase letters, digits, `-`, `_`, `.`
        name: String,
        /// The lane it starts from and lands on (default: main)
        #[arg(long, value_name = "LANE")]
        from: Option<String>,
    },
    /// Bring into a permanent lane what main did since it started from it
    #[command(name = "catch-up")]
    CatchUp { lane: String },
    /// Record the last changes of a lane and remove its working copy
    Close { lane: String },
    /// Ops since the base, files touched, open conflicts
    Status { lane: String },
    /// Run a command in the working copy of a lane, confined to it: it
    /// writes only there, and does not read Kito's own data or your keys
    Run {
        lane: String,
        /// The command opens no TCP connection either
        #[arg(long)]
        no_network: bool,
        /// The command and its arguments, after `--`
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Show what a lane declared it would change, or add to it
    Scope {
        lane: String,
        /// Functions, files or directories to add to the scope
        #[arg(long, num_args = 1.., value_name = "ITEM")]
        add: Vec<String>,
    },
    /// In a directory connected to a server: bring the files of a lane
    /// here and keep them in step as the lane changes
    Attach {
        lane: String,
        /// Work in the lane from this machine: what is saved here goes to
        /// the lane. For the lane's owner; takes over from another machine
        #[arg(long)]
        write: bool,
    },
    /// Remove this machine's copy of a lane; the lane itself stays
    Detach {
        lane: String,
        /// Also when saves made here did not reach the server: they are lost
        #[arg(long)]
        discard: bool,
    },
    /// Settle a file that was changed here and on the server in ways that
    /// do not merge
    Take {
        lane: String,
        /// The file, as a path inside the lane
        path: String,
        /// Keep this machine's file: it replaces the server's
        #[arg(long, conflicts_with = "theirs", required_unless_present = "theirs")]
        mine: bool,
        /// Keep the server's file: it replaces this machine's
        #[arg(long)]
        theirs: bool,
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

#[derive(Subcommand)]
enum HookCmd {
    /// Before an edit: may it be made? Reads what Claude Code says about
    /// the edit on the standard input; with --file, answers in words for
    /// any other tool and leaves with 2 when the edit should not be made
    BeforeEdit {
        /// The file about to be edited
        #[arg(long)]
        file: Option<PathBuf>,
        /// The lines about to be replaced, as 12 or 12-18 (default: the whole file)
        #[arg(long = "lines")]
        lines: Vec<String>,
    },
    /// After a shell command: what it changed where it should not have
    AfterShell,
    /// When a session starts: the rules that are enforced, and who is on what
    SessionStart,
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

/// Sets the project up for Claude Code, unless that was not wanted. The
/// files are written before the first scan, which records them.
fn claude_setup(root: &Path, no_claude: bool) -> Option<claude::Setup> {
    (!no_claude && !claude::disabled()).then(|| claude::setup(root))
}

fn cmd_claude(cli: &Cli) -> Outcome {
    let root = root_of(cli)?;
    // On a protected main lane the files would be put back as soon as
    // they are written: said before, not found out after.
    if Remote::load(&root).is_none() {
        let status: DaemonStatus = daemon(cli)?.call("status", json!({})).map_err(fail)?;
        if status.repo.protected {
            return Err("the main lane is protected, and the set-up writes files in the main directory: \
                        `kito protect --off`, `kito claude`, `kito checkpoint -m \"set up for Claude Code\"`, \
                        then `kito protect`"
                .into());
        }
    }
    let setup = claude::setup(&root);
    if cli.json {
        print_json(&setup.json());
    } else {
        setup.print();
    }
    Ok(())
}

/// A directory connected to a server holds no repository of its own.
fn not_connected(root: &Path) -> Outcome {
    match Remote::load(root) {
        Some(remote) => Err(format!(
            "this directory is connected to `{}` on {}: the repository is kept there, not here",
            remote.repo,
            remote.place()
        )),
        None => Ok(()),
    }
}

fn cmd_init(cli: &Cli, path: &Option<PathBuf>, fresh: bool, no_claude: bool) -> Outcome {
    let root = path.clone().or_else(|| cli.repo.clone()).unwrap_or_else(|| PathBuf::from("."));
    not_connected(&root)?;
    std::fs::create_dir_all(&root).map_err(fail)?;
    if root.join(".git").exists() && !fresh && !root.join(".kito").join("format").is_file() {
        return Err("this directory is a git repository: use `kito import` to bring its history along, \
                    or `kito init --fresh` to start from scratch"
            .to_owned());
    }
    let repo = Repo::init(&root).map_err(fail)?;
    let root = repo.root().to_owned();
    repo.close().map_err(fail)?;
    let setup = claude_setup(&root, no_claude);
    // Capture is continuous from the very start: the daemon starts with the repository.
    let mut client = Client::connect_or_start(&root).map_err(fail)?;
    if cli.json {
        print_json(&json!({ "root": root, "claude": setup.as_ref().map(claude::Setup::json) }));
    } else {
        println!("Kito repository created in {}", root.join(".kito").display());
        if let Some(setup) = &setup {
            setup.print();
        }
        println!("The daemon is running: every save is recorded from now on.");
        warn_if_events_unreliable(&mut client);
    }
    Ok(())
}

fn cmd_import(cli: &Cli, path: &Option<PathBuf>, no_claude: bool) -> Outcome {
    let root = path.clone().or_else(|| cli.repo.clone()).unwrap_or_else(|| PathBuf::from("."));
    let root = root.canonical().map_err(|e| format!("{}: {e}", root.display()))?;
    not_connected(&root)?;
    // The import works directly on the store: the daemon, if there is one, steps aside.
    stop_daemon(&root)?;
    let mut repo =
        if root.join(".kito").join("format").is_file() { Repo::open(&root) } else { Repo::init(&root) }
            .map_err(fail)?;
    let report = repo.import_git().map_err(fail)?;
    let setup = claude_setup(&root, no_claude);
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
            "claude": setup.as_ref().map(claude::Setup::json),
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
        if let Some(setup) = &setup {
            setup.print();
        }
        if !captured.is_empty() {
            println!("Captured {} not yet in git.", count(captured.len() as u64, "change", "changes"));
        }
        println!("The daemon is running: every save is recorded from now on.");
        warn_if_events_unreliable(&mut client);
    }
    Ok(())
}

/// Who did something, and for whom: `alice for ada`.
fn by_for(value: &Value) -> String {
    let by = value["by"].as_str().unwrap_or_default();
    match value["for"].as_str() {
        Some(person) if person != by => format!("{by} for {person}"),
        _ => by.to_owned(),
    }
}

/// The last reason of a neighbour, after its name.
fn last_reason(neighbour: &Value) -> String {
    let last = &neighbour["last"];
    match last["why"].as_str() {
        Some(why) => {
            let checkpoint = last["checkpoint"].as_str().map(|ck| format!(", {ck}")).unwrap_or_default();
            format!("  last changed for: {why} ({}{checkpoint})", by_for(last))
        }
        None => String::new(),
    }
}

/// The answer to `why` as a Mermaid diagram: the target, the reasons that
/// changed it and what changed with them, who uses it, who is on it now.
fn mermaid(answer: &Value) -> String {
    let clean = |text: &str| text.replace('"', "'").replace('\n', " ");
    let mut out = String::from("flowchart LR\n");
    let mut nodes: Vec<String> = Vec::new();
    // One node for each thing, however many times it is named.
    let mut node = |out: &mut String, label: &str, open: &str, close: &str| -> String {
        let at = nodes.iter().position(|known| known == label).unwrap_or_else(|| {
            nodes.push(label.to_owned());
            out.push_str(&format!("  n{}{open}\"{}\"{close}\n", nodes.len() - 1, clean(label)));
            nodes.len() - 1
        });
        format!("n{at}")
    };
    let target = &answer["target"];
    let name = match target["name"].as_str() {
        Some(name) => format!("{}:{name}", target["file"].as_str().unwrap_or_default()),
        None => target["file"].as_str().unwrap_or_default().to_owned(),
    };
    let me = node(&mut out, &name, "[", "]");
    for reason in answer["reasons"].as_array().into_iter().flatten() {
        let checkpoint = reason["checkpoint"].as_str().map(|ck| format!(" · {ck}")).unwrap_or_default();
        let label =
            format!("{}<br/>{}{checkpoint}", reason["why"].as_str().unwrap_or_default(), by_for(reason));
        let why = node(&mut out, &label, "([", "])");
        out.push_str(&format!("  {why} -->|{}| {me}\n", clean(&strings(&reason["how"]).join(", "))));
        for other in strings(&reason["with"]) {
            let other = node(&mut out, other, "[", "]");
            out.push_str(&format!("  {why} --> {other}\n"));
        }
    }
    for user in answer["used_by"].as_array().into_iter().flatten() {
        let user = node(&mut out, user["symbol"].as_str().unwrap_or_default(), "[", "]");
        out.push_str(&format!("  {user} -.->|uses| {me}\n"));
    }
    for used in answer["uses"].as_array().into_iter().flatten() {
        let used = node(&mut out, used["symbol"].as_str().unwrap_or_default(), "[", "]");
        out.push_str(&format!("  {me} -.->|uses| {used}\n"));
    }
    for lane in answer["in_progress"].as_array().into_iter().flatten() {
        let label = format!(
            "{} {}: {}",
            lane["lane"].as_str().unwrap_or_default(),
            lane["agent"].as_str().unwrap_or_default(),
            lane["intent"].as_str().unwrap_or_default()
        );
        let lane = node(&mut out, &label, "{{", "}}");
        out.push_str(&format!("  {lane} -.->|on it now| {me}\n"));
    }
    out
}

/// A target as the daemon reads it: the file as the repository names it.
fn why_target(root: &Path, target: &str, lane: &Option<String>) -> Result<String, String> {
    let (file, rest) =
        match target.split_once(':').filter(|(file, rest)| !file.is_empty() && !rest.is_empty()) {
            Some((file, rest)) => (file, Some(rest)),
            None => (target, None),
        };
    // In a lane, files are named as the lane's working copy has them.
    let file = match lane {
        Some(_) => PathBuf::from(file),
        None => repo_relative(root, Path::new(file))?,
    };
    Ok(match rest {
        Some(rest) => format!("{}:{rest}", file.display()),
        None => file.display().to_string(),
    })
}

fn cmd_why(cli: &Cli, target: &str, lane: &Option<String>, graph: bool, uses: bool) -> Outcome {
    let root = root_of(cli)?;
    let target = why_target(&root, target, lane)?;
    let mut client = daemon(cli)?;
    let mut params = json!({ "target": target, "uses": uses });
    if let Some(lane) = lane {
        params["lane"] = json!(lane);
    }
    let answer: Value = client.call("why_graph", params).map_err(fail)?;
    if cli.json {
        print_json(&answer);
        return Ok(());
    }
    if graph {
        print!("{}", mermaid(&answer));
        return Ok(());
    }
    let about = &answer["target"];
    let text = |value: &Value, key: &str| value[key].as_str().unwrap_or_default().to_owned();
    let lines = match (about["lines"][0].as_u64(), about["lines"][1].as_u64()) {
        (Some(from), Some(to)) if from != to => format!("  lines {from}-{to}"),
        (Some(from), _) => format!("  line {from}"),
        _ => String::new(),
    };
    match about["name"].as_str() {
        Some(name) => println!("{} {name}  ({}){lines}", text(about, "kind"), text(about, "file")),
        None => println!("file {}", text(about, "file")),
    }

    // What people and agents left on it: first, it is what they meant to be read.
    let annotations = answer["annotations"].as_array().cloned().unwrap_or_default();
    if !annotations.is_empty() {
        println!();
        println!("Notes left on it:");
        annotations.iter().for_each(|note| println!("  {}", note_line(note)));
    }

    let reasons = answer["reasons"].as_array().cloned().unwrap_or_default();
    println!();
    match reasons.is_empty() {
        true => println!("No reason is recorded for it."),
        false => println!("Why it is the way it is, newest first:"),
    }
    for (index, reason) in reasons.iter().enumerate() {
        let task = reason["task"].as_str().map(|task| format!("  ({task})")).unwrap_or_default();
        println!("  {}. {}{task}", index + 1, text(reason, "why"));
        let mut facts = vec![strings(&reason["how"]).join(", "), format!("by {}", by_for(reason))];
        facts.extend(reason["lane"].as_str().map(str::to_owned));
        match reason["checkpoint"].as_str() {
            Some(checkpoint) => facts.push(checkpoint.to_owned()),
            None => facts.push("not on the main lane yet".to_owned()),
        }
        facts.push(utc(reason["at_ms"].as_u64().unwrap_or(0)).chars().take(16).collect());
        println!("     {}", facts.join(" · "));
        let checks = strings(&reason["checks"]);
        let approved = reason["approved_by"].as_str().map(|person| format!("let in by {person}"));
        let passed = (!checks.is_empty()).then(|| format!("checks passed: {}", checks.join("; ")));
        let said: Vec<String> = passed.into_iter().chain(approved).collect();
        if !said.is_empty() {
            println!("     {}", said.join(" · "));
        }
        let with = strings(&reason["with"]);
        if !with.is_empty() {
            let more = match reason["with_more"].as_u64() {
                Some(more) if more > 0 => format!(" and {more} more"),
                _ => String::new(),
            };
            println!("     with: {}{more}", with.join(", "));
        }
    }

    // Its lines, each with the number of the reason it was last written for.
    let runs = answer["lines"].as_array().cloned().unwrap_or_default();
    if !runs.is_empty() {
        let file = PathBuf::from(text(about, "file"));
        let place = match lane {
            None => Some(root.clone()),
            Some(lane) => client
                .call::<Value>("lane_status", json!({ "lane": lane }))
                .ok()
                .and_then(|status| status["workdir"].as_str().map(PathBuf::from)),
        };
        let source = place.and_then(|dir| std::fs::read_to_string(dir.join(&file)).ok());
        let code: Vec<&str> = source.as_deref().map(|text| text.lines().collect()).unwrap_or_default();
        println!();
        println!("Its lines, with the reason each was last written for:");
        for run in &runs {
            let (from, to) = (run["from"].as_u64().unwrap_or(0), run["to"].as_u64().unwrap_or(0));
            let reason = run["reason"].as_u64().map_or("  ?".to_owned(), |index| format!("{:>3}", index + 1));
            match code.is_empty() {
                true => println!("  {reason}  {from}-{to}"),
                false => (from..=to).for_each(|line| {
                    let text = code.get(line as usize - 1).copied().unwrap_or_default();
                    println!("  {reason}  {line:>4}  {text}");
                }),
            }
        }
    }

    let listed = |title: &str, key: &str| {
        let items = answer[key].as_array().cloned().unwrap_or_default();
        if !items.is_empty() {
            println!();
            println!("{title}");
        }
        for item in &items {
            let line = item["line"].as_u64().map(|line| format!(" (line {line})")).unwrap_or_default();
            println!("  {}{line}{}", text(item, "symbol"), last_reason(item));
        }
    };
    listed("Used by:", "used_by");
    listed("Uses:", "uses");
    listed("It contains:", "contains");

    let lanes = answer["in_progress"].as_array().cloned().unwrap_or_default();
    if !lanes.is_empty() {
        println!();
        println!("On it now:");
    }
    for lane in &lanes {
        let whose = match lane["for"].as_str() {
            Some(person) if person != text(lane, "agent") => format!(" (of {person})"),
            _ => String::new(),
        };
        println!(
            "  {}  {}{whose}  {}  — {}",
            text(lane, "lane"),
            text(lane, "agent"),
            text(lane, "intent"),
            text(lane, "how")
        );
    }
    let decisions = answer["decisions"].as_array().cloned().unwrap_or_default();
    if !decisions.is_empty() {
        println!();
        println!("Decided:");
    }
    for decision in &decisions {
        let decided = match decision["decided"].as_str() {
            Some("main") => "kept what the main lane had".to_owned(),
            Some("lane") => "kept what the lane had".to_owned(),
            Some("text") => "a text written for the occasion".to_owned(),
            Some(other) => other.to_owned(),
            None => "not decided yet".to_owned(),
        };
        println!(
            "  {} ({}) with {}: {decided}",
            text(decision, "conflict"),
            text(decision, "kind"),
            text(decision, "lane")
        );
    }
    for note in strings(&answer["notes"]) {
        println!();
        println!("Note: {note}");
    }
    Ok(())
}

/// `note#3  src/shop.ts:total  (ada for bob): the text`.
fn note_line(note: &Value) -> String {
    let text = |key: &str| note[key].as_str().unwrap_or_default().to_owned();
    let who = match note["for"].as_str() {
        Some(person) => format!("{} for {person}", text("by")),
        None => text("by"),
    };
    let gone =
        if note["gone"].as_bool() == Some(true) { ", on a function that no longer exists" } else { "" };
    format!("{}  {}  ({who}{gone}): {}", text("id"), text("on"), text("text"))
}

/// Leaves a note on code, lists the notes, or removes one (ADR 0003).
fn cmd_note(
    cli: &Cli,
    target: &Option<String>,
    text: &[String],
    lane: &Option<String>,
    list: bool,
    remove: &Option<String>,
) -> Outcome {
    let mut client = daemon(cli)?;
    let shown: Value = match (remove, list, target) {
        (Some(note), _, _) => client.call("remove_note", json!({ "note": note })).map_err(fail)?,
        (None, true, _) | (None, false, None) => {
            client.call("notes", json!({ "on": target, "lane": lane })).map_err(fail)?
        }
        (None, false, Some(target)) if text.is_empty() => {
            return Err(fail(format!("what to say about {target}: kito note {target} \"<text>\"")));
        }
        (None, false, Some(target)) => client
            .call("add_note", json!({ "on": target, "text": text.join(" "), "lane": lane }))
            .map_err(fail)?,
    };
    if cli.json {
        print_json(&shown);
        return Ok(());
    }
    match (remove, shown.as_array()) {
        (Some(_), _) => println!("{} removed.", shown["id"].as_str().unwrap_or_default()),
        (None, Some(notes)) if notes.is_empty() => println!("No notes."),
        (None, Some(notes)) => notes.iter().for_each(|note| println!("{}", note_line(note))),
        (None, None) => {
            println!("{}", note_line(&shown));
            println!("It is told with the why of that code, to whoever is about to change it.");
        }
    }
    Ok(())
}

fn cmd_reasons(cli: &Cli, words: &[String], limit: usize) -> Outcome {
    let hits: Value =
        daemon(cli)?.call("reasons", json!({ "about": words.join(" "), "limit": limit })).map_err(fail)?;
    if cli.json {
        print_json(&hits);
        return Ok(());
    }
    let hits = hits.as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        println!("No reason has those words.");
    }
    for hit in &hits {
        let text = |key: &str| hit[key].as_str().unwrap_or_default().to_owned();
        let task = hit["task"].as_str().map(|task| format!("  ({task})")).unwrap_or_default();
        println!("{}{task}", text("why"));
        let by = strings(&hit["by"]).join(", ");
        let who = match hit["for"].as_str() {
            Some(person) if person != by => format!("{by} for {person}"),
            _ => by,
        };
        let mut facts = vec![format!("by {who}")];
        facts.extend(hit["lane"].as_str().map(str::to_owned));
        facts.push(hit["checkpoint"].as_str().map_or("not on the main lane yet".to_owned(), str::to_owned));
        facts.push(utc(hit["at_ms"].as_u64().unwrap_or(0)).chars().take(16).collect());
        println!("  {}", facts.join(" · "));
        let more = match hit["changed_more"].as_u64() {
            Some(more) if more > 0 => format!(" and {more} more"),
            _ => String::new(),
        };
        println!("  changed: {}{more}", strings(&hit["changed"]).join(", "));
    }
    Ok(())
}

fn cmd_history(cli: &Cli, target: &str, lane: &Option<String>) -> Outcome {
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

fn cmd_log(
    cli: &Cli,
    target: &Option<PathBuf>,
    limit: usize,
    lane: &Option<String>,
    verbose: bool,
) -> Outcome {
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
    let mut ask = |changes: usize| -> Result<Vec<LogItem>, String> {
        let mut params = json!({ "limit": changes, "path": rel });
        if let Some(lane) = &lane {
            params["lane"] = json!(lane);
        }
        client.call("log", params).map_err(fail)
    };
    if !verbose && !cli.json {
        // Several saves make one line: enough of them are asked for to
        // fill the lines wanted, and one more, so that the last line shown
        // is a whole run of saves.
        let mut changes = limit.saturating_mul(4).max(64);
        loop {
            let items = ask(changes)?;
            let whole = log::changes(&items) < changes;
            let (lines, all) = log::compact(items, limit);
            if all > limit || whole || changes >= 1 << 20 {
                if lines.is_empty() {
                    println!("No operations recorded.");
                }
                for line in lines {
                    println!("{line}");
                }
                return Ok(());
            }
            changes = changes.saturating_mul(4);
        }
    }
    let items = ask(limit)?;
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
    let abs = match abs.canonical() {
        Ok(resolved) => resolved,
        Err(_) => match (abs.parent().and_then(|p| p.canonical().ok()), abs.file_name()) {
            (Some(dir), Some(name)) => dir.join(name),
            _ => abs,
        },
    };
    Ok(abs
        .strip_prefix(root)
        .map(kito_core::paths::portable_path)
        .unwrap_or_else(|_| kito_core::paths::portable_path(path)))
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

fn cmd_scip(cli: &Cli, file: &Path) -> Outcome {
    // The daemon reads the file: it needs the whole path.
    let file = std::fs::canonicalize(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let done: Value = daemon(cli)?.call("import_scip", json!({ "file": file })).map_err(fail)?;
    if cli.json {
        print_json(&done);
        return Ok(());
    }
    println!(
        "Index imported: {} and the uses of {}.",
        count(done["files"].as_u64().unwrap_or(0), "file", "files"),
        count(done["symbols"].as_u64().unwrap_or(0), "symbol", "symbols")
    );
    if let Some(unknown) = done["unknown_files"].as_u64().filter(|n| *n > 0) {
        println!(
            "{} of the index are not in the repository, and were left out.",
            count(unknown, "file", "files")
        );
    }
    println!("It answers for each file until that file changes: import a new one now and then.");
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
        lane.state.replace('_', " "),
        lane.base,
        count(lane.ops, "op", "ops"),
        count(lane.files.len() as u64, "file", "files")
    );
    if !lane.conflicts.is_empty() {
        line.push_str(&format!("  {}", count(lane.conflicts.len() as u64, "conflict", "conflicts")));
    }
    if let Some(name) = &lane.name {
        line.push_str(&format!("  permanent: {name}"));
    }
    match (&lane.parent, &lane.target) {
        (Some(parent), Some(target)) if parent == target => line.push_str(&format!("  on {parent}")),
        (Some(parent), Some(target)) => line.push_str(&format!("  on {parent}, lands on {target}")),
        (None, Some(target)) => line.push_str(&format!("  lands on {target}")),
        _ => {}
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
        println!("No open lanes. Open one with `kito lane open <agent> --intent <why> --scope <what>`.");
    } else {
        lanes.iter().for_each(|lane| println!("{}", lane_line(lane)));
    }
    Ok(())
}

/// Brings in the commits of the git remote. Fails (after saying why) when
/// they wait in a lane for somebody to settle something.
fn cmd_pull(cli: &Cli) -> Outcome {
    let done: Value = daemon(cli)?.call("pull", json!({})).map_err(fail)?;
    if cli.json {
        print_json(&done);
    } else {
        println!("{}", done["message"].as_str().unwrap_or_default());
        if let Ok(result) = serde_json::from_value::<IntegrationResult>(done["result"].clone()) {
            if result.result != Integration::Integrated {
                print_integration(&result);
            }
        }
    }
    match done["lane"].is_null() {
        true => Ok(()),
        false => Err(String::new()),
    }
}

/// Runs a command in the working copy of a lane, confined to it. Returns
/// its exit code (128 and more: stopped by a signal).
fn cmd_lane_run(cli: &Cli, lane: &str, network: bool, command: &[String]) -> Result<u8, String> {
    let root = root_of(cli)?;
    let status: Value = daemon(cli)?.call("lane_status", json!({ "lane": lane })).map_err(fail)?;
    let workdir = status["workdir"].as_str().map(PathBuf::from).ok_or_else(|| {
        format!("{lane} has no working copy on this machine: `kito lane attach {lane} --write`")
    })?;
    // Where the command was asked from, if that is inside the lane.
    let here = std::env::current_dir().ok().and_then(|dir| dir.canonical().ok());
    let inside = here.filter(|dir| workdir.canonical().is_ok_and(|work| dir.starts_with(work)));
    let (program, args) = command.split_first().ok_or("no command to run")?;
    let mut confined = kitod::sandbox::lane_command(&root, &workdir, network)?;
    if let Some(dir) = inside {
        confined.current_dir(dir);
    }
    let status = confined.arg(program).args(args).status().map_err(|e| format!("{program}: {e}"))?;
    Ok(status.code().map_or(137, |code| code.clamp(0, 255) as u8))
}

/// The strings of a JSON list.
fn strings(value: &Value) -> Vec<&str> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).collect()
}

/// Why what a lane declared is the way it is, as the daemon told it when
/// it was declared.
fn print_scope_why(answer: &Value) {
    let told: Vec<kitod::guard::ScopeWhy> = serde_json::from_value(answer["why"].clone()).unwrap_or_default();
    if !told.is_empty() {
        println!("Why it is the way it is:");
    }
    for item in &told {
        println!("  {}", item.item);
        for reason in &item.reasons {
            println!("    {}", kitod::guard::line_of(reason));
        }
        if item.earlier > 0 {
            println!(
                "    and {} before: `kito why {}`",
                count(item.earlier as u64, "reason", "reasons"),
                item.item
            );
        }
        if !item.used_by.is_empty() {
            let more = match item.used_by_more {
                0 => String::new(),
                more => format!(" and {more} more"),
            };
            println!("    used by {}{more}", item.used_by.join(", "));
        }
        for decision in &item.decisions {
            if let Some(decided) = &decision.decided {
                println!("    decided ({}): {decided}", decision.conflict);
            }
        }
    }
    if let Some(note) = answer["why_note"].as_str() {
        println!("  ({note})");
    }
}

/// The other lanes on the same code, as the daemon told them.
fn print_overlaps(overlaps: &Value) {
    for overlap in overlaps.as_array().into_iter().flatten() {
        let text = |key: &str| overlap[key].as_str().unwrap_or_default();
        match text("kind") {
            "out_of_scope" => println!("  outside the scope: {}", text("what")),
            "landed" => {
                let why = overlap["intent"].as_str().map(|why| format!(" ({why})")).unwrap_or_default();
                println!("  landed on main: {}{why}", text("detail"));
            }
            kind => {
                let why = match overlap["intent"].as_str() {
                    Some(intent) => format!(", there for: {intent}"),
                    None => String::new(),
                };
                // Whose agent it is, when it is somebody's.
                let whose = match overlap["owner"].as_str() {
                    Some(owner) if owner != text("agent") => format!(" ({owner})"),
                    _ => String::new(),
                };
                println!("  overlap ({kind}) with {} of {}{whose}{why}", text("lane"), text("agent"));
                // What a caller overlap is on is already a sentence.
                match kind {
                    "caller" => println!("    {}", text("detail")),
                    _ => println!("    {}: {}", text("what"), text("detail")),
                }
            }
        }
    }
}

/// What a lane declared, and what it changed outside of it.
fn print_scope(status: &Value) {
    match strings(&status["scope"]).as_slice() {
        [] => println!("Scope ............. none declared: `kito lane scope <lane> --add <item>`"),
        scope => println!("Scope ............. {}", scope.join(", ")),
    }
    let outside = strings(&status["out_of_scope"]);
    for item in &outside {
        println!("  outside the scope  {item}");
    }
    if !outside.is_empty() {
        println!("  The lane is not integrated until the scope is extended to them, or they are undone.");
    }
}

/// How a server given on the command line is reached: `host:port` over
/// TCP with a token, `ssh://user@host/dir` through SSH, where the person is
/// who SSH says.
fn remote_for(server: &str, repo: &str, token: &Option<String>) -> Result<Remote, String> {
    if let Some(rest) = server.strip_prefix("ssh://") {
        let (host, dir) = rest
            .split_once('/')
            .filter(|(host, dir)| !host.is_empty() && !dir.is_empty())
            .ok_or("give the server as ssh://user@host/path/of/the/server")?;
        let command = ["ssh", host, "kito-server", "--dir", &format!("/{dir}"), "stdio"];
        return Ok(Remote {
            command: command.iter().map(|part| (*part).to_owned()).collect(),
            repo: repo.to_owned(),
            ..Remote::default()
        });
    }
    let token = token
        .clone()
        .or_else(|| std::env::var("KITO_TOKEN").ok())
        .filter(|token| !token.trim().is_empty())
        .ok_or("a server reached as host:port wants a token: --token, or $KITO_TOKEN")?;
    Ok(Remote {
        server: Some(server.to_owned()),
        repo: repo.to_owned(),
        token: Some(token),
        ..Remote::default()
    })
}

/// Says how the server is known to be the one meant, before anything is
/// sent to it: by the fingerprint given, by an authority, or, for a server
/// on this machine, by the certificate it presents now, kept from then on.
fn trusted(
    mut remote: Remote,
    fingerprint: &Option<String>,
    authority: bool,
    plain: bool,
) -> Result<Remote, String> {
    if remote.server.is_none() {
        // Through SSH: it encrypts, and knows its hosts by their keys.
        return Ok(remote);
    }
    remote.fingerprint = fingerprint.clone();
    remote.authority = authority;
    remote.plain = plain;
    if plain || authority || remote.fingerprint.is_some() {
        return Ok(remote);
    }
    let (address, host) = remote.address().map_err(fail)?;
    let presented = std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(10))
        .map_err(|e| format!("cannot reach the server: {}: {e}", remote.place()))
        .and_then(|stream| {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
            kitod::tls::presented(stream, &host).map_err(|e| format!("{}: {e}", remote.place()))
        })?;
    if address.ip().is_loopback() {
        remote.fingerprint = Some(presented);
        return Ok(remote);
    }
    Err(format!(
        "{} is on another machine, and nothing says it is the server you mean. It presents a certificate \
         with fingerprint\n  {presented}\nIf that is what `kito-server fingerprint` prints on the server, \
         connect again with\n  --fingerprint {presented}\n(or with --authority if its certificate comes \
         from a public authority).",
        remote.place()
    ))
}

fn cmd_connect(cli: &Cli, remote: Remote, repo: &str, dir: &Option<PathBuf>, no_claude: bool) -> Outcome {
    let dir = dir.clone().unwrap_or_else(|| PathBuf::from(repo));
    if dir.join(".kito").join("format").is_file() {
        return Err(format!("{} is a repository of its own: connect another directory", dir.display()));
    }
    // Asked first: a connection the server refuses leaves nothing behind.
    let mut client = Client::remote(&remote, true).map_err(fail)?;
    let me: Value = client.call("server.whoami", json!({})).map_err(fail)?;
    std::fs::create_dir_all(&dir).map_err(fail)?;
    remote.save(&dir).map_err(fail)?;
    let dir = dir.canonical().map_err(fail)?;
    // The main lane's files come first, so that the directory is the
    // project; then what Claude Code needs, added to what the project has.
    let main: Result<Value, String> = Client::connect_or_start(&dir)
        .and_then(|mut client| client.call("attach", json!({ "lane": "main" })))
        .map_err(fail);
    let setup = claude_setup(&dir, no_claude);
    if cli.json {
        print_json(&json!({
            "dir": dir,
            "repo": repo,
            "server": remote.place(),
            "user": me["user"],
            "files": main.as_ref().ok().map(|main| main["files"].clone()),
            "claude": setup.as_ref().map(claude::Setup::json),
        }));
    } else {
        println!(
            "{} is connected to `{repo}` on {} as {}.",
            dir.display(),
            remote.place(),
            me["user"].as_str().unwrap_or("?")
        );
        match (&remote.fingerprint, remote.authority, remote.plain) {
            (Some(print), ..) => println!(
                "The connection is encrypted, and the server is known by its certificate ({print}): one that \
                 presents another is refused."
            ),
            (None, true, _) => println!("The connection is encrypted, and the server is known by its name."),
            (None, false, true) => println!("The connection is NOT encrypted (--plain)."),
            _ => {}
        }
        match &main {
            Ok(main) => println!(
                "The main lane is in it ({}), kept as the server has it: to change something, open a lane.",
                count(main["files"].as_u64().unwrap_or(0), "file", "files")
            ),
            Err(e) => println!(
                "The files of the main lane did not get here ({e}): `kito lane attach main` tries again."
            ),
        }
        println!("A lane you open is edited on this machine and recorded on the server;");
        println!("`kito lane attach <lane>` brings the files of another's lane here, kept in step.");
        if let Some(setup) = &setup {
            setup.print();
        }
    }
    Ok(())
}

fn cmd_repos(cli: &Cli) -> Outcome {
    let root = root_of(cli)?;
    let remote = Remote::load(&root)
        .ok_or("this directory is a repository of its own: `kito repos` is for one connected to a server")?;
    let repos: Vec<String> =
        Client::remote(&remote, false).map_err(fail)?.call("server.repos", json!({})).map_err(fail)?;
    if cli.json {
        print_json(&json!({ "server": remote.place(), "repos": repos, "connected": remote.repo }));
        return Ok(());
    }
    for repo in repos {
        let here = if repo == remote.repo { "  (this directory)" } else { "" };
        println!("{repo}{here}");
    }
    Ok(())
}

fn cmd_watch(cli: &Cli, lane: &Option<String>, count: Option<usize>, past: bool) -> Outcome {
    use std::io::Write;
    let mut client = daemon(cli)?;
    let mut after = match past {
        true => 0,
        false => {
            let now: Value = client.call("events", json!({})).map_err(fail)?;
            now["next"].as_u64().unwrap_or(0)
        }
    };
    let mut shown = 0;
    loop {
        // A request that waits for what happens next.
        let heard: Value =
            client.call("events", json!({ "after": after, "wait_ms": 25_000 })).map_err(fail)?;
        after = heard["next"].as_u64().unwrap_or(after);
        for event in heard["events"].as_array().into_iter().flatten() {
            let text = |key: &str| event[key].as_str().unwrap_or_default();
            // `3`, `lane/3` and `main` all name a lane.
            let bare = |lane: &str| lane.strip_prefix("lane/").unwrap_or(lane).to_owned();
            if lane.as_deref().is_some_and(|only| bare(only) != bare(text("lane"))) {
                continue;
            }
            if cli.json {
                println!("{event}");
            } else {
                let when = utc(event["at_ms"].as_u64().unwrap_or(0));
                let by = match text("by") {
                    "" => String::new(),
                    by => format!("  by {by}"),
                };
                let what = match text("what") {
                    "" => String::new(),
                    what => format!("  {what}"),
                };
                println!(
                    "{}  {:<8}  {}{by}{what}",
                    when.get(11..19).unwrap_or(&when),
                    text("lane"),
                    text("kind").replace('_', " ")
                );
            }
            let _ = std::io::stdout().flush();
            shown += 1;
            if count.is_some_and(|limit| shown >= limit) {
                return Ok(());
            }
        }
    }
}

fn cmd_board(cli: &Cli) -> Outcome {
    let board: Value = daemon(cli)?.call("board", json!({})).map_err(fail)?;
    if cli.json {
        print_json(&board);
        return Ok(());
    }
    let lanes = board.as_array().cloned().unwrap_or_default();
    if lanes.is_empty() {
        println!("No open lanes.");
    }
    for lane in &lanes {
        let text = |key: &str| lane[key].as_str().unwrap_or_default();
        let whose = match lane["owner"].as_str() {
            Some(owner) if owner != text("agent") => format!(" (of {owner})"),
            _ => String::new(),
        };
        let state = match (text("state"), lane["proposed_by"].as_str()) {
            ("awaiting_approval", Some(by)) => format!("awaiting approval (asked by {by})"),
            (state, _) => state.replace('_', " "),
        };
        println!("{}  {}{whose}  {state}  {}", text("id"), text("agent"), text("intent"));
        let scope: Vec<&str> =
            lane["scope"].as_array().into_iter().flatten().filter_map(|item| item["item"].as_str()).collect();
        let broad = match lane["broad"].as_bool() {
            Some(true) => format!("  (broad: {} files)", lane["scope_files"].as_u64().unwrap_or(0)),
            _ => String::new(),
        };
        println!(
            "  scope    {}{broad}",
            if scope.is_empty() { "none declared".to_owned() } else { scope.join(", ") }
        );
        let changed = strings(&lane["changed"]);
        if !changed.is_empty() {
            println!("  changed  {}", changed.join(", "));
        }
        for item in strings(&lane["out_of_scope"]) {
            println!("  outside the scope: {item}");
        }
        print_overlaps(&lane["overlaps"]);
    }
    Ok(())
}

fn cmd_who(cli: &Cli, target: &str) -> Outcome {
    let found: Value = daemon(cli)?.call("who_touches", json!({ "target": target })).map_err(fail)?;
    if cli.json {
        print_json(&found);
        return Ok(());
    }
    let lanes = found.as_array().cloned().unwrap_or_default();
    if lanes.is_empty() {
        println!("No open lane declared or changed {target}.");
    }
    for overlap in &lanes {
        let text = |key: &str| overlap[key].as_str().unwrap_or_default();
        let why = match overlap["intent"].as_str() {
            Some(intent) => format!(", there for: {intent}"),
            None => String::new(),
        };
        let whose = match overlap["owner"].as_str() {
            Some(owner) if owner != text("agent") => format!(", {owner}'s"),
            _ => String::new(),
        };
        println!("{} of {}{whose} ({}){why}", text("lane"), text("agent"), text("kind"));
        match text("kind") {
            "caller" => println!("  {}", text("detail")),
            _ => println!("  {}: {}", text("what"), text("detail")),
        }
    }
    Ok(())
}

/// The copies of lanes on this machine are a connected directory's.
fn connected(cli: &Cli) -> Outcome {
    match Remote::load(&root_of(cli)?) {
        Some(_) => Ok(()),
        None => {
            Err("this directory is a repository of its own: its lanes are here already (`kito lanes`)".into())
        }
    }
}

/// What a copy of a lane on this machine is waiting for, if anything.
fn print_mirror(mirror: &Value) {
    let pending = mirror["pending"].as_u64().unwrap_or(0);
    if pending > 0 {
        let why = mirror["problem"].as_str().map(|why| format!(": {why}")).unwrap_or_default();
        println!("    {} not on the server yet{why}", count(pending, "save", "saves"));
    }
    if let Some(note) = mirror["note"].as_str() {
        println!("    {note}");
    }
    for path in strings(&mirror["diverged"]) {
        let lane = mirror["lane"].as_str().unwrap_or_default();
        println!(
            "    {path} differs here and on the server: `kito lane take {lane} {path} --mine` or `--theirs`"
        );
    }
    for (path, reason) in mirror["refused"].as_object().into_iter().flatten() {
        println!("    not sent: {path}: {}", reason.as_str().unwrap_or_default());
    }
    for path in strings(&mirror["held"]) {
        println!("    {path} was changed here and is not sent: this copy only follows");
    }
}

fn cmd_lane(cli: &Cli, command: &LaneCmd) -> Outcome {
    if matches!(command, LaneCmd::Attach { .. } | LaneCmd::Detach { .. } | LaneCmd::Take { .. }) {
        connected(cli)?;
    }
    let mut client = daemon(cli)?;
    match command {
        LaneCmd::Attach { lane, write } => {
            let mirror: Value =
                client.call("attach", json!({ "lane": lane, "write": write })).map_err(fail)?;
            if cli.json {
                print_json(&mirror);
                return Ok(());
            }
            let text = |key: &str| mirror[key].as_str().unwrap_or_default();
            println!("{} is here: {}", text("lane"), text("workdir"));
            match write {
                true => println!("What is saved there goes to the lane; what the lane gets on the server comes back."),
                false => println!("It follows the lane: its files change as the lane's do. What is saved there stays there."),
            }
            print_mirror(&mirror);
        }
        LaneCmd::Detach { lane, discard } => {
            let gone: Value =
                client.call("detach", json!({ "lane": lane, "discard": discard })).map_err(fail)?;
            if cli.json {
                print_json(&gone);
            } else if gone["removed"] == false {
                println!("This directory no longer follows the main lane; its files stay as they are.");
            } else {
                println!(
                    "This machine's copy of {} is gone; the lane is as it was on the server.",
                    gone["lane"].as_str().unwrap_or(lane)
                );
            }
        }
        LaneCmd::Take { lane, path, mine, .. } => {
            let done: Value =
                client.call("take", json!({ "lane": lane, "path": path, "mine": mine })).map_err(fail)?;
            if cli.json {
                print_json(&done);
            } else {
                println!("{}", done["done"].as_str().unwrap_or_default());
            }
        }
        LaneCmd::Open { agent, intent, scope, task, base, on, target } => {
            let opened: Value = client
                .call(
                    "open_lane",
                    json!({
                        "agent": agent, "task": task, "base": base, "intent": intent, "scope": scope,
                        "on": on, "target": target,
                    }),
                )
                .map_err(fail)?;
            if cli.json {
                print_json(&opened);
            } else {
                let lane: LaneInfo = serde_json::from_value(opened.clone()).map_err(fail)?;
                match (&lane.parent, &lane.target) {
                    (Some(parent), Some(target)) if parent == target => {
                        println!("{} opened for {} on {parent}, which it lands on.", lane.id, lane.agent)
                    }
                    (Some(parent), Some(target)) => println!(
                        "{} opened for {} on {parent}; it lands on {target} once {parent} has.",
                        lane.id, lane.agent
                    ),
                    _ => println!("{} opened for {} on {}.", lane.id, lane.agent, lane.base),
                }
                println!("Working copy: {}", lane.workdir.as_deref().unwrap_or(Path::new("")).display());
                println!("Scope: {}", lane.scope.join(", "));
                print_overlaps(&opened["overlaps"]);
                print_scope_why(&opened);
                println!(
                    "Every save there is recorded on the lane; `kito integrate {}` brings it in.",
                    lane.id
                );
            }
        }
        LaneCmd::Create { name, from } => {
            let created: Value =
                client.call("create_lane", json!({ "name": name, "from": from })).map_err(fail)?;
            if cli.json {
                print_json(&created);
            } else {
                let lane: LaneInfo = serde_json::from_value(created).map_err(fail)?;
                println!(
                    "{name} ({}) created on {}: open lanes on it with `--on {name}`, land it on main with `kito \
                     integrate {name}`.",
                    lane.id, lane.base
                );
            }
        }
        LaneCmd::CatchUp { lane } => {
            let done: Value = client.call("catch_up", json!({ "lane": lane })).map_err(fail)?;
            if cli.json {
                print_json(&done);
            } else {
                let files: Vec<&str> =
                    done["files"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                match files.is_empty() {
                    true => println!("{lane} already has everything main has."),
                    false => println!("{lane} caught up with main: {}", files.join(", ")),
                }
            }
        }
        // Run before any of these: its exit code is the command's.
        LaneCmd::Run { .. } => {}
        LaneCmd::Scope { lane, add } => {
            if !add.is_empty() {
                let grown: Value =
                    client.call("extend_scope", json!({ "lane": lane, "scope": add })).map_err(fail)?;
                if cli.json {
                    print_json(&grown);
                    return Ok(());
                }
                print_overlaps(&grown["overlaps"]);
                print_scope_why(&grown);
            }
            let status: Value = client.call("lane_status", json!({ "lane": lane })).map_err(fail)?;
            if cli.json {
                print_json(
                    &json!({ "lane": status["id"], "scope": status["scope"], "out_of_scope": status["out_of_scope"] }),
                );
                return Ok(());
            }
            print_scope(&status);
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
            if status["parent"].is_string() {
                println!("On ................ {}", text("parent"));
            }
            if status["target"].is_string() {
                println!("Lands on .......... {}", text("target"));
            }
            match status["workdir"].as_str() {
                Some(workdir) => println!("Working copy ...... {workdir}"),
                // A lane of a repository on a server, not worked on from here.
                None if status["state"] != "closed" => println!(
                    "Working copy ...... on the server (`kito lane attach {}` brings its files here)",
                    text("id")
                ),
                None => println!("Working copy ...... -"),
            }
            println!("Ops since the base  {}", status["ops"].as_u64().unwrap_or(0));
            for file in status["files"].as_array().into_iter().flatten() {
                println!("  changed  {}", file.as_str().unwrap_or_default());
            }
            print_scope(&status);
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
        .map(|v| match v.batch.len() {
            0 | 1 => format!(", verified in {:.1} s", v.duration_ms as f64 / 1000.0),
            // The other lanes of the run are named once, on the first line.
            together => format!(
                ", verified in {:.1} s together with {} other {}",
                v.duration_ms as f64 / 1000.0,
                together - 1,
                if together == 2 { "lane" } else { "lanes" }
            ),
        })
        .unwrap_or_default();
    match result.result {
        Integration::Integrated => {
            let git = result
                .git_commit
                .as_ref()
                .map(|c| format!(" (git {})", &c[..10.min(c.len())]))
                .unwrap_or_default();
            match (&result.landed_on, &result.checkpoint) {
                (Some(target), Some(checkpoint)) => {
                    println!("{}: landed on {target} as {checkpoint}{git}{verified}", result.lane)
                }
                (Some(target), None) => println!("{}: landed on {target}", result.lane),
                (None, _) => println!(
                    "{}: integrated as {}{git}{verified}",
                    result.lane,
                    result.checkpoint.clone().unwrap_or_default()
                ),
            }
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
        Integration::AwaitingApproval => {
            println!(
                "{}: merged and checked{verified}; it waits for somebody who may integrate",
                result.lane
            );
            println!(
                "  They bring it in with `kito integrate {}`. Until then the lane is as it is.",
                result.lane
            );
        }
        Integration::OutOfScope => {
            println!("{}: not integrated, it changed things outside its scope", result.lane);
            for item in &result.out_of_scope {
                println!("  outside the scope  {item}");
            }
            println!(
                "  Declare them with `kito lane scope {} --add <item>`, or undo them, then integrate again.",
                result.lane
            );
        }
    }
}

fn cmd_integrate(cli: &Cli, lanes: &[String]) -> Outcome {
    let mut client = daemon(cli)?;
    let mut results = Vec::new();
    // Asked all at once: lanes that do not depend on each other are then
    // checked in one run of the repository's checks.
    let mut jobs = Vec::new();
    for lane in lanes {
        let asked: Value = client.call("request_integration", json!({ "lane": lane })).map_err(fail)?;
        jobs.push(asked["job"].as_str().unwrap_or_default().to_owned());
    }
    for job in jobs {
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
    // A lane that waits for approval got as far as its asker could take it.
    let done = |outcome: Integration| {
        use Integration::{AwaitingApproval, Integrated, NothingToIntegrate};
        matches!(outcome, Integrated | NothingToIntegrate | AwaitingApproval)
    };
    match results.iter().all(|r| done(r.result)) {
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
    match repo.protected {
        true => println!("Main lane ......... protected: it changes only when a lane is integrated"),
        false => println!("Main lane ......... open: what is saved in the main directory is recorded"),
    }
    if let Some(last) = repo.last_refused.as_ref().filter(|_| repo.protected) {
        let kept = last.kept.as_ref().map(|dir| format!(", kept in {}", dir.display())).unwrap_or_default();
        println!(
            "                    {} put back since the daemon started; the last: {}{kept}",
            count(repo.refused, "direct edit", "direct edits"),
            last.paths.join(", ")
        );
    }
    match repo.checks.is_empty() {
        true => println!("Checks ............ none: a lane is integrated as soon as it merges"),
        false => println!("Checks ............ {}", repo.checks.join("; ")),
    }
    // How the checks run matters once there are some.
    if !repo.checks.is_empty() {
        let how = match repo.sandbox.as_str() {
            "confined" => "confined: they write only in their own copy and open no network connection",
            "confined, with network" => "confined: they write only in their own copy",
            "off" => "NOT confined: the repository turned it off ([sandbox] mode)",
            "unavailable" => "NOT confined: this system cannot (it needs Linux 5.13 or later)",
            _ => "",
        };
        if !how.is_empty() {
            println!("                    {how}");
        }
    }
    if let Some(remote) = &repo.push {
        let state = match (&repo.push_error, &repo.pushed) {
            (Some(error), _) => format!("the last push failed: {error}"),
            (None, Some(checkpoint)) => format!("{checkpoint} is there"),
            (None, None) => "nothing pushed yet by this daemon".to_owned(),
        };
        println!("Git push .......... {remote} ({state})");
        if let Some(lane) = &repo.pull_waiting {
            println!(
                "                    commits made with git wait to be merged in {lane}: `kito conflicts`, then \
                 `kito pull`"
            );
        }
    }
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

fn cmd_protect(cli: &Cli, on: bool) -> Outcome {
    if Remote::load(&root_of(cli)?).is_some() {
        return Err(
            "the rules of a repository on a server are set there: `kito-server repo set <name> --protect`"
                .into(),
        );
    }
    let done: Value = daemon(cli)?.call("protect", json!({ "on": on })).map_err(fail)?;
    if cli.json {
        print_json(&done);
        return Ok(());
    }
    match (on, done["changed"].as_bool().unwrap_or(false)) {
        (true, true) => {
            println!("The main lane is protected: it changes only when a lane is integrated.");
            println!(
                "A file edited in the main directory is put back, and the edit kept under .kito/refused/."
            );
        }
        (true, false) => println!("The main lane was protected already."),
        (false, true) => {
            println!("The main lane is open again: what is saved in the main directory is recorded.")
        }
        (false, false) => println!("The main lane was not protected."),
    }
    Ok(())
}

/// The daemon of a connected directory: the server it asks, and the lanes
/// it keeps here.
fn cmd_mirrors(cli: &Cli) -> Outcome {
    let status: Value = daemon(cli)?.call("mirror_status", json!({})).map_err(fail)?;
    if cli.json {
        print_json(&status);
        return Ok(());
    }
    let text = |key: &str| status[key].as_str().unwrap_or("-").to_owned();
    println!("Directory ......... {}", text("root"));
    println!("Connected to ...... `{}` on {}", text("repo"), text("connected_to"));
    println!("Daemon ............ pid {}, version {}", status["pid"], text("version"));
    let mirrors = status["mirrors"].as_array().cloned().unwrap_or_default();
    println!("Lanes kept here ... {}", mirrors.len());
    for mirror in &mirrors {
        let text = |key: &str| mirror[key].as_str().unwrap_or_default();
        let role = match (mirror["closed"].as_bool(), mirror["write"].as_bool()) {
            (Some(true), _) => "closed",
            (_, Some(true)) => "writes",
            _ => "follows",
        };
        println!(
            "  {:<10} {:<12} {:<8} {:>8}  {}",
            text("lane"),
            text("agent"),
            role,
            count(mirror["files"].as_u64().unwrap_or(0), "file", "files"),
            text("workdir")
        );
        print_mirror(mirror);
    }
    Ok(())
}

fn cmd_status(cli: &Cli, with_daemon: bool) -> Outcome {
    if with_daemon && Remote::load(&root_of(cli)?).is_some() {
        return cmd_mirrors(cli);
    }
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
    // `kito log | head`: whoever was reading stopped, which is no failure
    // of ours and needs no report.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        if panic.to_string().contains("failed printing to stdout") {
            std::process::exit(0);
        }
        report(panic);
    }));
    let cli = Cli::parse();
    // A hook answers whoever runs it in its own words and with its own code.
    if let Cmd::Hook { what } = &cli.command {
        return match what {
            HookCmd::BeforeEdit { file, lines } => hook::before_edit(file.as_deref(), lines),
            HookCmd::AfterShell => hook::after_shell(),
            HookCmd::SessionStart => hook::session_start(),
        };
    }
    // So does a command run in a lane: its exit code is the command's.
    if let Cmd::Lane { command: LaneCmd::Run { lane, no_network, command } } = &cli.command {
        return match cmd_lane_run(&cli, lane, !*no_network, command) {
            Ok(code) => ExitCode::from(code),
            Err(e) => {
                eprintln!("kito: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let outcome = match &cli.command {
        Cmd::Init { path, fresh, no_claude } => cmd_init(&cli, path, *fresh, *no_claude),
        Cmd::Import { path, no_claude } => cmd_import(&cli, path, *no_claude),
        Cmd::Claude => cmd_claude(&cli),
        Cmd::Log { target, limit, lane, verbose } => cmd_log(&cli, target, *limit, lane, *verbose),
        Cmd::Checkpoint { label } => cmd_checkpoint(&cli, label),
        Cmd::ExportGit => cmd_export(&cli),
        Cmd::Scip { file } => cmd_scip(&cli, file),
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
        Cmd::Why { target, lane, history: true, .. } => cmd_history(&cli, target, lane),
        Cmd::Why { target, lane, graph, uses, .. } => cmd_why(&cli, target, lane, *graph, *uses),
        Cmd::Reasons { words, limit } => cmd_reasons(&cli, words, *limit),
        Cmd::Note { target, text, lane, list, remove } => cmd_note(&cli, target, text, lane, *list, remove),
        Cmd::Sync { lane } => cmd_sync(&cli, lane),
        Cmd::Lanes { all } => cmd_lanes(&cli, *all),
        Cmd::Lane { command } => cmd_lane(&cli, command),
        Cmd::Board => cmd_board(&cli),
        Cmd::Connect { server, name, dir, token, fingerprint, authority, plain, no_claude } => {
            let how = remote_for(server, name, token)
                .and_then(|remote| trusted(remote, fingerprint, *authority, *plain));
            match how {
                Ok(remote) => cmd_connect(&cli, remote, name, dir, *no_claude),
                Err(e) => Err(e),
            }
        }
        Cmd::Repos => cmd_repos(&cli),
        Cmd::Watch { lane, count, past } => cmd_watch(&cli, lane, *count, *past),
        Cmd::Who { target } => cmd_who(&cli, target),
        Cmd::Integrate { lanes } => cmd_integrate(&cli, lanes),
        Cmd::Conflicts { id, all } => cmd_conflicts(&cli, id, *all),
        Cmd::Resolve { conflict, main, lane, text, rewrite, keep } => {
            let choice = [(*main, "main"), (*lane, "lane"), (*rewrite, "rewrite"), (*keep, "keep")];
            cmd_resolve(&cli, conflict, choice.iter().find(|(set, _)| *set).map(|(_, word)| *word), text)
        }
        Cmd::Rename { symbol, new_name, lane } => cmd_rename(&cli, symbol, new_name, lane),
        Cmd::Intent { summary, lane, task } => cmd_intent(&cli, summary, lane, task),
        Cmd::Hook { .. } => Ok(()),
        Cmd::Pull => cmd_pull(&cli),
        Cmd::Protect { off } => cmd_protect(&cli, !*off),
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
    use super::{remote_for, utc};

    #[test]
    fn a_server_is_reached_with_a_token_or_through_ssh() {
        let tcp = remote_for("kito.example.com:7421", "shop", &Some("kito_abc".into())).unwrap();
        assert_eq!(
            (tcp.server.as_deref(), tcp.token.as_deref()),
            (Some("kito.example.com:7421"), Some("kito_abc"))
        );
        assert!(tcp.command.is_empty());
        let ssh = remote_for("ssh://ada@kito.example.com/srv/kito", "shop", &None).unwrap();
        assert_eq!(
            ssh.command,
            ["ssh", "ada@kito.example.com", "kito-server", "--dir", "/srv/kito", "stdio"]
        );
        assert_eq!((ssh.server, ssh.token, ssh.repo.as_str()), (None, None, "shop"));
        assert!(remote_for("ssh://ada@host", "shop", &None).is_err());
    }

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00");
        assert_eq!(utc(1_794_659_467_000), "2026-11-14 12:31:07");
        assert_eq!(utc(4_102_444_799_999), "2099-12-31 23:59:59");
    }
}
