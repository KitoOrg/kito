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
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod assistants;
mod claude;
mod claude_out;
mod hook;
mod instructions;
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
enum PluginCmd {
    /// Install a plugin for you: by its name, from the registry, or from
    /// a directory that holds its `kito-plugin.toml` and its program
    Add {
        /// The plugin, as the registry lists it: `web`, or `web@0.4.0`
        #[arg(required_unless_present = "path", conflicts_with = "path")]
        name: Option<String>,
        /// A directory with a plugin built or unpacked here
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
    },
    /// What the registry has: every plugin, or those that say these words
    Search { words: Vec<String> },
    /// The plugins you installed that the registry has a later version of
    Outdated,
    /// Install the later version of a plugin, or of every plugin that has one
    Update { name: Option<String> },
    /// Remove a plugin you installed: it takes out what it wrote for you
    /// outside any repository, then its program goes
    Remove {
        name: String,
        /// Do not run the plugin one last time
        #[arg(long)]
        no_cleanup: bool,
    },
    /// The plugins you installed, and what this repository says of each
    List,
    /// What a plugin is, asks for and adds
    Info { name: String },
    /// Let a plugin run in this repository, with what it asks for
    Enable {
        name: String,
        /// Give it these permissions only: read, write, integrate, admin
        #[arg(long, value_delimiter = ',', value_name = "PERMISSIONS")]
        only: Option<Vec<String>>,
    },
    /// Stop a plugin from running in this repository: it takes out what
    /// it wrote here, and its key is gone
    Disable {
        name: String,
        /// Remove what the plugin kept of its own here as well
        #[arg(long)]
        purge: bool,
        /// Do not run the plugin one last time
        #[arg(long)]
        no_cleanup: bool,
    },
}

#[derive(Subcommand)]
enum RegistryCmd {
    /// Make a key for a registry: the file is the secret that signs it,
    /// and what is printed is what everybody checks it with
    Key {
        /// Where the secret is written: kept off the machine that serves
        /// the registry
        file: PathBuf,
    },
    /// Sign a registry kept in a directory: its index gets the next
    /// number, and every file a signature beside it
    Sign {
        /// The directory that holds index.json and plugins/
        dir: PathBuf,
        /// The file `kito registry key` wrote
        #[arg(long, value_name = "FILE")]
        key: PathBuf,
    },
}

#[derive(Subcommand)]
enum AssistantCmd {
    /// Set coding assistants up for Kito in this repository, the same way
    /// for each: claude, codex, zcode, or any a plugin knows. The plugin
    /// that knows one is installed and enabled here when needed
    Add {
        #[arg(required = true, value_name = "NAME")]
        names: Vec<String>,
    },
    /// Take out what `add` wrote in this repository
    Remove { name: String },
    /// The assistants Kito knows, which this machine has, and how each is set up
    List,
    /// Which assistants every new project gets (`kito init`, `import`,
    /// `connect`); without a name, says it
    Default {
        #[arg(value_name = "NAME", conflicts_with = "none")]
        names: Vec<String>,
        /// New projects get none
        #[arg(long)]
        none: bool,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// The tasks still to do (`--all`: done and dropped too)
    List {
        #[arg(long)]
        all: bool,
        /// Only those for this person or agent
        #[arg(long = "for", value_name = "WHO")]
        assignee: Option<String>,
    },
    /// A task, its lanes and what was said on it
    Show { task: String },
    /// Ask for something to be done
    Add {
        #[arg(required = true)]
        title: Vec<String>,
        /// What it is about, in more words
        #[arg(long, conflicts_with = "file")]
        text: Option<String>,
        /// The same, from a file
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Who it is for: a person or an agent
        #[arg(long = "for", value_name = "WHO")]
        assignee: Option<String>,
    },
    /// Say something on a task: the agents in its lanes are told
    Comment {
        task: String,
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Say who a task is for (nobody, without a name)
    Assign { task: String, who: Option<String> },
    /// The task is done
    Done { task: String },
    /// It is not done after all
    Reopen { task: String },
    /// Nobody will do it
    Drop { task: String },
}

#[derive(Subcommand)]
enum PeopleCmd {
    /// Add a person to the server: their token is printed once
    Add {
        name: String,
        /// member (the default), admin, owner
        #[arg(long, default_value = "member")]
        role: String,
        #[arg(long)]
        email: Option<String>,
        /// Who they are for a way to sign in, as <method>:<login>: they
        /// sign in, and have no token
        #[arg(long, value_name = "METHOD:LOGIN")]
        identity: Option<String>,
    },
    /// Remove a person from the server
    Remove { name: String },
    /// Change a person's role on the server: member, admin, owner
    Role { name: String, role: String },
}

#[derive(Subcommand)]
enum InstructionsCmd {
    /// What every assistant is told here, and where each part comes from
    List,
    /// What an assistant is told, as its file gives it
    Show {
        /// claude, codex, zcode
        #[arg(long = "for", value_name = "ASSISTANT", default_value = "claude")]
        assistant: String,
    },
    /// Set one of this repository's instructions, from a file or the
    /// standard input: `all/<name>` for every assistant, `<assistant>/<name>`
    /// for one. It replaces the kitchen's of the same name; an empty one
    /// takes it away
    Set {
        name: String,
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// Remove one of this repository's instructions (the kitchen's of the
    /// same name is told again)
    Remove { name: String },
    /// Attach a kitchen: a directory with an `instructions/` folder
    Kitchen {
        #[arg(value_name = "DIR", required_unless_present = "none", conflicts_with = "none")]
        dir: Option<PathBuf>,
        /// The standard kitchen: Kito's rules alone
        #[arg(long)]
        none: bool,
    },
    /// Write the files the assistants read now (it is done at every session)
    Write,
}

#[derive(Subcommand)]
enum Cmd {
    /// Coding assistants (Claude Code, Codex, ZCode, …): `kito assistant add <name>`
    Assistant {
        #[command(subcommand)]
        command: AssistantCmd,
    },
    /// Search what was done here: reasons, lanes, tasks, comments, notes, changes
    Search {
        words: Vec<String>,
        /// reason, lane, task, comment, note, change (again for more; default: all)
        #[arg(long = "kind", value_name = "KIND")]
        kinds: Vec<String>,
        /// From when: 7d, 12h, 2w, or a day (2026-11-01)
        #[arg(long)]
        since: Option<String>,
        /// Until when, as --since
        #[arg(long)]
        until: Option<String>,
        /// A person, an agent, an assistant or a model
        #[arg(long)]
        who: Option<String>,
        /// A file, a directory (src/) or a function (file:name)
        #[arg(long = "where", value_name = "PLACE")]
        place: Option<String>,
        /// integrated, given_up, open, refused, conflicted, failed_checks
        #[arg(long)]
        outcome: Option<String>,
        #[arg(long, default_value = "30")]
        limit: usize,
        /// Every repository you may use on the server this directory is connected to
        #[arg(long)]
        everywhere: bool,
    },
    /// The numbers: how lanes flow, each assistant and model, busy code, review
    Stats {
        /// From when: 7d, 12h, 2w, or a day (2026-11-01)
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        /// Every repository you may use on the server this directory is connected to
        #[arg(long)]
        everywhere: bool,
    },
    /// Tasks: what needs doing here, and the lanes that do it
    Task {
        #[command(subcommand)]
        command: Option<TaskCmd>,
    },
    /// Who may use the repository this directory is connected to, and as what
    Members,
    /// Let a person use this repository: read, write, integrate, maintain
    /// (maintainers and admins)
    Grant { name: String, role: String },
    /// Take this repository away from a person
    Revoke { name: String },
    /// The people of the server this directory is connected to (admins)
    People {
        #[command(subcommand)]
        command: Option<PeopleCmd>,
    },
    /// What the assistants are told here: kept by Kito and shared, not in
    /// the project's files
    Instructions {
        #[command(subcommand)]
        command: Option<InstructionsCmd>,
    },
    /// Plugins: programs that add to Kito (a web client, an assistant)
    Plugin {
        #[command(subcommand)]
        command: PluginCmd,
    },
    /// A command of a plugin this repository enabled: `kito web`
    #[command(external_subcommand)]
    External(Vec<OsString>),
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
        /// Set this assistant up here (again for more), instead of the
        /// ones `kito assistant default` says or the question
        #[arg(long = "assistant", value_name = "NAME", conflicts_with = "no_assistants")]
        assistants: Vec<String>,
        /// Set no assistant up here
        #[arg(long)]
        no_assistants: bool,
    },
    /// Import the current state and the history of a git repository
    Import {
        /// Git repository to import (default: the current directory)
        path: Option<PathBuf>,
        /// Do not set the project up for Claude Code (see `kito claude`)
        #[arg(long)]
        no_claude: bool,
        /// Set this assistant up here (again for more)
        #[arg(long = "assistant", value_name = "NAME", conflicts_with = "no_assistants")]
        assistants: Vec<String>,
        /// Set no assistant up here
        #[arg(long)]
        no_assistants: bool,
    },
    /// Set the project up for Claude Code: the MCP server, the permission to
    /// call it, the rules in CLAUDE.md (the same as `kito assistant add claude`)
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
    /// Connect a directory to a repository kept by a Kito server:
    /// `kito connect kito.example.com/shop`
    #[command(visible_alias = "clone")]
    Connect {
        /// The repository's address: host[:port]/repository (or
        /// kito://host[:port]/repository; the port is 7421 when none is
        /// said). Also the server alone, then the repository: host:port
        /// shop, ssh://user@host/path/of/the/server shop
        #[arg(value_name = "ADDRESS")]
        server: String,
        /// The repository, when the address is of the server alone; else
        /// the directory
        #[arg(value_name = "REPO")]
        name: Option<String>,
        /// The directory to connect (default: a new one named as the repository)
        dir: Option<PathBuf>,
        /// The token the server gave you, for host:port (default: $KITO_TOKEN)
        #[arg(long)]
        token: Option<String>,
        /// Sign in instead, in a way the server offers (`github`): you
        /// are sent to a page to say it is you, and need no token
        #[arg(long, value_name = "METHOD", conflicts_with = "token")]
        sign_in: Option<String>,
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
        /// Set this assistant up here (again for more)
        #[arg(long = "assistant", value_name = "NAME", conflicts_with = "no_assistants")]
        assistants: Vec<String>,
        /// Set no assistant up here
        #[arg(long)]
        no_assistants: bool,
    },
    /// Sign in again to the server this directory is connected to: when
    /// the session ended, or to be somebody else
    SignIn {
        /// The way to sign in (default: the one this directory was connected with)
        method: Option<String>,
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
    /// A registry of plugins of your own: its key, and its signature
    Registry {
        #[command(subcommand)]
        command: RegistryCmd,
    },
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
        /// The assistant the agent runs in (claude-code, codex): shown
        /// with the lane. An assistant set up for Kito says it by itself
        #[arg(long, value_name = "NAME")]
        assistant: Option<String>,
        /// With --assistant: the model it runs (claude-opus-5)
        #[arg(long, value_name = "MODEL", requires = "assistant")]
        model: Option<String>,
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
    /// Say something about a lane, or about code in it: its agent is told
    Comment {
        lane: String,
        text: String,
        /// What it is about: file:name, a file, or lines (src/shop.ts:12-14)
        #[arg(long, conflicts_with = "reply")]
        on: Option<String>,
        /// The comment it answers (comment#3)
        #[arg(long)]
        reply: Option<String>,
    },
    /// What was said about a lane and is not resolved
    Comments {
        lane: String,
        /// The resolved ones too
        #[arg(long)]
        all: bool,
    },
    /// Mark a comment as dealt with: it is kept, and no longer told
    #[command(name = "resolve-comment")]
    ResolveComment { comment: String },
    /// Approve a lane as it is now. It brings nothing in: `kito integrate` does
    Approve { lane: String, text: Option<String> },
    /// Say that a lane should change before it goes in, and how
    #[command(name = "request-changes")]
    RequestChanges { lane: String, text: String },
    /// Take back the verdict you gave on a lane
    Withdraw { lane: String },
    /// Where the review of a lane stands: verdicts, what is missing
    Review { lane: String },
    /// What a lane changes: files, functions and lines (ADR 0004)
    Diff {
        lane: String,
        /// Only these files and directories
        paths: Vec<String>,
        /// The files and the functions, without the lines
        #[arg(long)]
        stat: bool,
    },
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
        /// The assistant that asks (codex, zcode): the lane the edit is
        /// in says it. Claude Code is known by itself
        #[arg(long, value_name = "NAME")]
        assistant: Option<String>,
        /// The model it runs, when the assistant says it
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Its session, as the assistant names it: what the session said
        /// when it started is said of the lane too
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// After a shell command: what it changed where it should not have
    AfterShell,
    /// When a session starts: the rules that are enforced, and who is on what
    SessionStart {
        /// The assistant that starts (codex, zcode). Claude Code is known by itself
        #[arg(long, value_name = "NAME")]
        assistant: Option<String>,
    },
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

/// One line about a plugin: what it is, and what this repository says.
fn plugin_line(name: &str, version: &str, description: &str, here: Option<&Value>) -> String {
    let state = match here {
        Some(here) if here["enabled"] == true => {
            format!("enabled here ({})", strings(&here["permissions"]).join(", "))
        }
        Some(_) => "disabled here".to_owned(),
        None => "not enabled here".to_owned(),
    };
    let version = if version.is_empty() { String::new() } else { format!(" {version}") };
    let about = if description.is_empty() { String::new() } else { format!("  {description}") };
    format!("{name}{version}  [{state}]{about}")
}

fn cmd_plugin(cli: &Cli, command: &PluginCmd) -> Outcome {
    use kitod::plugins;
    // What the repository says, if this is one.
    let here = || -> Vec<Value> {
        let listed: Option<Value> =
            daemon(cli).ok().and_then(|mut client| client.call("plugins", json!({})).ok());
        listed.and_then(|listed| listed.as_array().cloned()).unwrap_or_default()
    };
    match command {
        PluginCmd::Add { name, path } => {
            // From a directory, or by its name from the registry (ADR 0008).
            let (added, source) = match (name, path) {
                (_, Some(path)) => (plugins::install_from(path)?, None),
                (Some(name), None) => {
                    let (name, version) = match name.split_once('@') {
                        Some((name, version)) => (name, Some(version)),
                        None => (name.as_str(), None),
                    };
                    let (added, source) = kitod::registry::install(name, version)?;
                    (added, Some(source))
                }
                (None, None) => return Err("say which plugin: its name, or --path <dir>".to_owned()),
            };
            let manifest = &added.manifest;
            if cli.json {
                print_json(&json!({
                    "name": manifest.name, "version": manifest.version, "dir": added.dir, "from": source,
                }));
                return Ok(());
            }
            println!("{} {} is installed in {}", manifest.name, manifest.version, added.dir.display());
            if let Some(source) = &source {
                println!("  from {} (SHA-256 {})", source.url, source.sha256);
                match source.signed {
                    true => println!("  the registry's files were checked against its key"),
                    false => println!(
                        "  the registry was not checked against a key: nothing but its address says these files are its own"
                    ),
                }
            }
            println!("  `kito plugin enable {}` lets it run in a repository.", manifest.name);
        }
        PluginCmd::Outdated => {
            let found = kitod::registry::outdated()?;
            if cli.json {
                print_json(&found);
                return Ok(());
            }
            if found.is_empty() {
                println!("Every plugin you installed is the latest the registry has for this Kito.");
            }
            for plugin in &found {
                println!("{} {} -> {}", plugin.name, plugin.installed, plugin.latest);
            }
            if !found.is_empty() {
                println!("`kito plugin update` installs them.");
            }
        }
        PluginCmd::Update { name } => {
            let found = kitod::registry::outdated()?;
            let wanted: Vec<_> =
                found.iter().filter(|plugin| name.as_ref().is_none_or(|name| *name == plugin.name)).collect();
            if let (Some(name), true) = (name, wanted.is_empty()) {
                return match plugins::installed(name) {
                    Some(found) => {
                        println!(
                            "{name} {} is the latest the registry has for this Kito.",
                            found.manifest.version
                        );
                        Ok(())
                    }
                    None => Err(format!("the plugin `{name}` is not installed: `kito plugin add {name}`")),
                };
            }
            if wanted.is_empty() {
                println!("Every plugin you installed is the latest the registry has for this Kito.");
            }
            // One that does not install is said, and the others go on.
            let mut failed = Vec::new();
            for plugin in wanted {
                match kitod::registry::install(&plugin.name, Some(&plugin.latest)) {
                    Ok((_, source)) => println!(
                        "{} {} -> {} (SHA-256 {})",
                        plugin.name, plugin.installed, plugin.latest, source.sha256
                    ),
                    Err(why) => failed.push(format!("{}: {why}", plugin.name)),
                }
            }
            if !failed.is_empty() {
                return Err(failed.join("\n"));
            }
        }
        PluginCmd::Search { words } => {
            let index = kitod::registry::index()?;
            let words: Vec<String> = words.iter().map(|word| word.to_lowercase()).collect();
            let says = |name: &str, about: &str| {
                let text = format!("{name} {about}").to_lowercase();
                words.iter().all(|word| text.contains(word))
            };
            let found: Vec<_> =
                index.plugins.iter().filter(|(name, listed)| says(name, &listed.about)).collect();
            if cli.json {
                let found: Vec<Value> = found
                    .iter()
                    .map(|(name, listed)| json!({ "name": name, "about": listed.about, "latest": listed.latest }))
                    .collect();
                print_json(&json!({ "registry": kitod::registry::base(), "plugins": found }));
                return Ok(());
            }
            match (found.is_empty(), index.plugins.is_empty()) {
                (true, true) => println!("The registry ({}) lists no plugin yet.", kitod::registry::base()),
                (true, false) => {
                    println!("The registry ({}) has no plugin that says that.", kitod::registry::base())
                }
                _ => {}
            }
            for (name, listed) in found {
                println!("{name} {}  {}", listed.latest, listed.about);
            }
        }
        PluginCmd::Remove { name, no_cleanup } => {
            let found =
                plugins::installed(name).ok_or_else(|| format!("the plugin `{name}` is not installed"))?;
            // One last time, for what it wrote outside any repository.
            if !*no_cleanup {
                said_cleaning(name, "for you", plugins::cleanup(&found, "person", &found.dir, &[]));
            }
            let still =
                here().iter().any(|plugin| plugin["name"] == name.as_str() && plugin["enabled"] == true);
            plugins::uninstall(name)?;
            println!("{name} is removed.");
            match still {
                true => println!(
                    "This repository still enables it, and says that it is missing: `kito plugin disable {name}`."
                ),
                false => println!("Repositories that enabled it say that it is missing."),
            }
        }
        PluginCmd::List => {
            let (installed, here) = (plugins::all_installed(), here());
            let said = |name: &str| here.iter().find(|plugin| plugin["name"] == name);
            if cli.json {
                let listed: Vec<Value> = installed
                    .iter()
                    .map(|plugin| {
                        json!({ "name": plugin.manifest.name, "version": plugin.manifest.version, "here": said(&plugin.manifest.name) })
                    })
                    .collect();
                print_json(&listed);
                return Ok(());
            }
            if installed.is_empty() && here.is_empty() {
                println!(
                    "No plugins. `kito plugin add <name>` installs one (`kito plugin search` lists them)."
                );
            }
            for plugin in &installed {
                let manifest = &plugin.manifest;
                println!(
                    "{}",
                    plugin_line(
                        &manifest.name,
                        &manifest.version,
                        &manifest.description,
                        said(&manifest.name)
                    )
                );
            }
            for plugin in &here {
                if let Some(failed) = plugin["last_failure"].as_str() {
                    println!("{} last failed {failed}", plugin["name"].as_str().unwrap_or_default());
                }
            }
            // What the repository relies on and this person does not have.
            for plugin in
                here.iter().filter(|plugin| plugin["installed"] == false && plugin["enabled"] == true)
            {
                println!("{}", plugin["problem"].as_str().unwrap_or_default());
            }
        }
        PluginCmd::Info { name } => {
            let plugin =
                plugins::installed(name).ok_or_else(|| format!("the plugin `{name}` is not installed"))?;
            let manifest = &plugin.manifest;
            if cli.json {
                print_json(manifest);
                return Ok(());
            }
            let here = here();
            let said = here.iter().find(|plugin| plugin["name"] == name.as_str());
            println!("{}", plugin_line(&manifest.name, &manifest.version, &manifest.description, said));
            println!("  program: {}", plugins::program(&plugin.dir, name).display());
            println!("  written for API {}", manifest.api);
            let asks: Vec<&str> = manifest.permissions.iter().map(|permission| permission.word()).collect();
            println!("  asks for: {}", if asks.is_empty() { "nothing".to_owned() } else { asks.join(", ") });
            for command in &manifest.commands {
                println!("  kito {}  {}", command.name, command.about);
            }
            for gate in &manifest.gates {
                println!("  gate {}  {}", gate.name, gate.about);
            }
            for tool in &manifest.tools {
                let about = tool.description.lines().next().unwrap_or_default();
                println!("  tool kito.{}.{}  {about}", manifest.name, tool.name);
            }
            if let Some(assistant) = &manifest.assistant {
                println!(
                    "  sets up the assistant {}: `kito assistant add {}`",
                    assistant.name, assistant.name
                );
            }
            let kinds: Vec<&str> =
                manifest.events.iter().flat_map(|event| event.kinds.iter().map(String::as_str)).collect();
            if !kinds.is_empty() {
                println!("  run for: {}", kinds.join(", "));
            }
            if let Some(failed) = said.and_then(|plugin| plugin["last_failure"].as_str()) {
                println!("  it last failed {failed}");
            }
            if let Some(why) = manifest.unfit() {
                println!("  {why}");
            }
        }
        PluginCmd::Enable { name, only } => {
            let enabled: Value = daemon(cli)?
                .call("plugin_enable", json!({ "name": name, "permissions": only }))
                .map_err(fail)?;
            if cli.json {
                print_json(&enabled);
                return Ok(());
            }
            let given = strings(&enabled["permissions"]);
            let given = if given.is_empty() { "nothing".to_owned() } else { given.join(", ") };
            println!("{name} is enabled in this repository. It may: {given}.");
            for command in enabled["commands"].as_array().into_iter().flatten() {
                println!(
                    "  kito {}  {}",
                    command["name"].as_str().unwrap_or_default(),
                    command["about"].as_str().unwrap_or_default()
                );
            }
        }
        PluginCmd::Disable { name, purge, no_cleanup } => {
            let mut client = daemon(cli)?;
            // One last time, while it still has its key: what it wrote
            // in the project comes out with it.
            if !*no_cleanup {
                let listed: Value = client.call("plugins", json!({})).unwrap_or(Value::Null);
                let plugin = listed.as_array().into_iter().flatten().find(|plugin| {
                    plugin["name"] == name.as_str() && plugin["enabled"] == true && plugin["cleanup"] == true
                });
                if let Some(program) = plugin.and_then(|plugin| plugin["program"].as_str()) {
                    let root = root_of(cli)?;
                    let key: Value = client.call("plugin_key", json!({ "name": name })).map_err(fail)?;
                    let ran = plugins::run_plugin(
                        Path::new(program),
                        &["cleanup", "repository"],
                        &root,
                        key["key"].as_str().unwrap_or_default(),
                        &[],
                        plugins::CLEANUP_WITHIN,
                    );
                    let ran = ran.map(|printed| String::from_utf8_lossy(&printed).trim().to_owned());
                    said_cleaning(name, "here", Some(ran));
                }
            }
            let done: Value =
                client.call("plugin_disable", json!({ "name": name, "purge": purge })).map_err(fail)?;
            match (done["changed"] == true, purge) {
                (true, true) => {
                    println!("{name} is disabled in this repository, and what it kept here is removed.")
                }
                (true, false) => println!("{name} is disabled in this repository."),
                (false, _) => println!("{name} was not enabled in this repository."),
            }
        }
    }
    Ok(())
}

/// `kito registry key | sign`: for whoever keeps a registry (ADR 0009).
fn cmd_registry(cli: &Cli, command: &RegistryCmd) -> Outcome {
    use std::io::Write;
    match command {
        RegistryCmd::Key { file } => {
            let (secret, public) = kitod::registry::new_key()?;
            // Never written over, and for its owner only: it signs for everybody.
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            options
                .open(file)
                .and_then(|mut out| writeln!(out, "{secret}"))
                .map_err(|e| format!("{}: {e} (a key is never written over)", file.display()))?;
            if cli.json {
                print_json(&json!({ "public": public, "secret_file": file }));
                return Ok(());
            }
            println!(
                "The secret is in {}: whoever has it signs for this registry. Keep it off the machine",
                file.display()
            );
            println!("that serves the registry, and keep a copy: without it the registry needs a new key.");
            println!("What everybody checks the registry with:");
            println!("{public}");
            println!("  KITO_REGISTRY_KEY={public}");
        }
        RegistryCmd::Sign { dir, key } => {
            let secret = std::fs::read_to_string(key).map_err(|e| format!("{}: {e}", key.display()))?;
            let secret = secret.trim();
            let public =
                kitod::registry::public_of(secret).map_err(|why| format!("{}: {why}", key.display()))?;
            let (serial, plugins) = kitod::registry::sign_dir(dir, secret)?;
            if cli.json {
                print_json(&json!({ "serial": serial, "plugins": plugins, "public": public }));
                return Ok(());
            }
            println!(
                "{} is signed: its index is number {serial}, with {}.",
                dir.display(),
                count(plugins as u64, "plugin", "plugins")
            );
            println!("Checked with the key {public}");
        }
    }
    Ok(())
}

/// Says how a plugin's last run went: what it printed, or that it failed,
/// which stops nothing. A broken plugin is not the one that cannot go.
fn said_cleaning(name: &str, place: &str, ran: Option<Result<String, String>>) {
    match ran {
        None => {}
        Some(Ok(printed)) => {
            for line in printed.lines() {
                println!("  {line}");
            }
        }
        Some(Err(why)) => {
            eprintln!("kito: the plugin `{name}` did not take out what it wrote {place} ({why}): it goes all the same")
        }
    }
}

/// Runs a program of an enabled plugin in the repository, with what it
/// needs to reach the daemon in its environment. Returns its exit code.
fn run_plugin_here(
    root: &Path,
    client: &mut Client,
    plugin: &Value,
    args: &[OsString],
    then_back: bool,
) -> Result<u8, String> {
    use kitod::plugins;
    let name = plugin["name"].as_str().unwrap_or_default();
    if let Some(problem) = plugin["problem"].as_str() {
        return Err(problem.to_owned());
    }
    let program = plugin["program"].as_str().map(PathBuf::from).ok_or("the plugin has no program")?;
    let key: Value = client.call("plugin_key", json!({ "name": name })).map_err(fail)?;
    let mut command = std::process::Command::new(&program);
    command
        .args(args)
        .current_dir(root)
        .env(plugins::ENV_REPO, root)
        .env(plugins::ENV_API, kitod::rpc::API.to_string())
        .env(plugins::ENV_KEY, key["key"].as_str().unwrap_or_default())
        .env(plugins::ENV_SOCKET, plugins::socket_name(root));
    // On Unix `kito` becomes the plugin: whoever stops one stops the
    // other, and a plugin that serves (a web client) is not left behind.
    // Unless Kito has something to do after it (`then_back`): an
    // assistant's files are written again once its plugin is done.
    #[cfg(unix)]
    if !then_back {
        let failed = std::os::unix::process::CommandExt::exec(&mut command);
        return Err(format!("{}: {failed}", program.display()));
    }
    let _ = then_back;
    let status = command.status().map_err(|e| format!("{}: {e}", program.display()))?;
    Ok(status.code().map_or(128, |code| code.clamp(0, 255) as u8))
}

/// `kito assistant`: every assistant is set up the same way (ADR 0011):
/// Claude Code by Kito itself, the others by the plugin that knows them.
fn cmd_assistant(cli: &Cli, command: &AssistantCmd) -> Result<u8, String> {
    match command {
        AssistantCmd::Default { names, none } => return assistant_default(cli, names, *none),
        AssistantCmd::List => return assistant_list(cli),
        AssistantCmd::Add { names } => return assistant_add(cli, names),
        AssistantCmd::Remove { name } if name == "claude" => return assistant_remove_claude(cli),
        AssistantCmd::Remove { .. } => {}
    }
    let root = root_of(cli)?;
    let mut client = Client::connect_or_start(&root).map_err(fail)?;
    let listed: Value = client.call("plugins", json!({})).map_err(fail)?;
    let all: &[Value] = listed.as_array().map_or(&[], Vec::as_slice);
    let sets_up =
        |name: &str| all.iter().find(|plugin| plugin["enabled"] == true && plugin["assistant"] == name);
    let (name, what) = match command {
        AssistantCmd::List => {
            let known: Vec<&Value> = all
                .iter()
                .filter(|plugin| plugin["enabled"] == true && plugin["assistant"].is_string())
                .collect();
            if cli.json {
                print_json(&known);
            } else if known.is_empty() {
                println!(
                    "No plugin enabled here sets an assistant up. Claude Code is set up by `kito claude`."
                );
            }
            for plugin in known.iter().filter(|_| !cli.json) {
                let text = |key: &str| plugin[key].as_str().unwrap_or_default();
                println!("{}  (plugin {} {})", text("assistant"), text("name"), text("version"));
            }
            return Ok(0);
        }
        AssistantCmd::Remove { name } => (name, "remove"),
        AssistantCmd::Add { .. } | AssistantCmd::Default { .. } => unreachable!("answered above"),
    };
    let Some(plugin) = sets_up(name) else {
        // Installed, and not let in here: say how.
        let has = kitod::plugins::all_installed().into_iter().find(|plugin| {
            plugin.manifest.assistant.as_ref().is_some_and(|assistant| assistant.name == *name)
        });
        return Err(match has {
            Some(plugin) => format!(
                "`{name}` is set up by the plugin `{}`, which this repository did not enable: `kito plugin \
                 enable {}`",
                plugin.manifest.name, plugin.manifest.name
            ),
            None => format!(
                "no plugin enabled here sets `{name}` up: install the one that does (`kito plugin add`), \
                 then `kito plugin enable` it"
            ),
        });
    };
    run_plugin_here(&root, &mut client, plugin, &["assistant".into(), what.into()], true)
}

/// `kito assistant add <name>…`: each the same way, said as it goes.
fn assistant_add(cli: &Cli, names: &[String]) -> Result<u8, String> {
    let root = root_of(cli)?;
    let mut said = Vec::new();
    let mut failed = 0;
    // A plugin enabled here that sets Claude Code up is asked, as for any
    // other assistant; otherwise Kito does it itself.
    let claude_by_plugin = Client::connect_or_start(&root)
        .ok()
        .and_then(|mut client| client.call::<Value>("plugins", json!({})).ok())
        .and_then(|listed| listed.as_array().cloned())
        .is_some_and(|listed| {
            listed.iter().any(|plugin| plugin["enabled"] == true && plugin["assistant"] == "claude")
        });
    for name in names {
        let done: Result<Value, String> = match name.as_str() {
            "claude" if !claude_by_plugin => claude_here(cli, &root).map(|setup| {
                if !cli.json {
                    setup.print();
                }
                setup.json()
            }),
            _ => Client::connect_or_start(&root)
                .map_err(fail)
                .and_then(|mut client| add_by_plugin(&root, &mut client, name))
                .map(|lines| {
                    if !cli.json {
                        println!("Set up for {}:", assistants::title(name));
                        lines.iter().for_each(|line| println!("  {line}"));
                    }
                    json!(lines)
                }),
        };
        match done {
            Ok(value) => said.push(json!({ "name": name, "set_up": true, "said": value })),
            Err(why) => {
                failed += 1;
                if !cli.json {
                    eprintln!("kito: {} is not set up: {why}", assistants::title(name));
                }
                said.push(json!({ "name": name, "set_up": false, "error": why }));
            }
        }
    }
    if cli.json {
        print_json(&said);
    }
    Ok(u8::from(failed > 0))
}

/// `kito assistant remove claude`: by the plugin when one is enabled here,
/// by Kito itself otherwise.
fn assistant_remove_claude(cli: &Cli) -> Result<u8, String> {
    let root = root_of(cli)?;
    let mut client = Client::connect_or_start(&root).map_err(fail)?;
    let listed: Value = client.call("plugins", json!({})).map_err(fail)?;
    let plugin = listed
        .as_array()
        .into_iter()
        .flatten()
        .find(|plugin| plugin["enabled"] == true && plugin["assistant"] == "claude")
        .cloned();
    if let Some(plugin) = plugin {
        return run_plugin_here(&root, &mut client, &plugin, &["assistant".into(), "remove".into()], true);
    }
    let done = claude_out::remove(&root)?;
    if cli.json {
        let files: Vec<Value> =
            done.iter().map(|one| json!({ "file": one.path, "change": one.what })).collect();
        print_json(&json!({ "files": files }));
        return Ok(0);
    }
    if done.is_empty() {
        println!("Claude Code was not set up for Kito here: nothing to take out.");
    } else {
        println!("Claude Code is no longer set up for Kito here:");
        for one in &done {
            println!("  {:<8}  {}", one.what, one.path.strip_prefix(&root).unwrap_or(&one.path).display());
        }
    }
    Ok(0)
}

/// `kito assistant list`: what Kito knows, what this machine has, and how
/// each is set up.
fn assistant_list(cli: &Cli) -> Result<u8, String> {
    let listed: Vec<Value> = root_of(cli)
        .ok()
        .and_then(|root| Client::connect_or_start(&root).ok())
        .and_then(|mut client| client.call::<Value>("plugins", json!({})).ok())
        .and_then(|listed| listed.as_array().cloned())
        .unwrap_or_default();
    let installed = kitod::plugins::all_installed();
    let defaults = assistants::defaults();
    let mut rows = Vec::new();
    let mut names: Vec<String> = assistants::KNOWN.iter().map(|one| one.name.to_owned()).collect();
    for plugin in &listed {
        if let Some(name) =
            plugin["assistant"].as_str().filter(|name| !names.iter().any(|known| known == name))
        {
            names.push(name.to_owned());
        }
    }
    for name in &names {
        let known = assistants::known(name);
        let found = known.and_then(assistants::found);
        let enabled =
            listed.iter().find(|plugin| plugin["enabled"] == true && plugin["assistant"] == name.as_str());
        let has = installed.iter().find(|plugin| {
            plugin.manifest.assistant.as_ref().is_some_and(|assistant| assistant.name == *name)
        });
        let how = match (known.map(|one| one.plugin), enabled, has) {
            (_, Some(plugin), _) => {
                format!("plugin {}, enabled here", plugin["name"].as_str().unwrap_or_default())
            }
            (Some(None), _, _) => "Kito itself".to_owned(),
            (_, None, Some(plugin)) => format!("plugin {}, installed", plugin.manifest.name),
            (Some(Some(plugin)), None, None) => format!("plugin {plugin}, from the registry"),
            (None, None, None) => "a plugin".to_owned(),
        };
        let by_default = defaults.as_ref().is_some_and(|list| list.iter().any(|one| one == name));
        rows.push(json!({
            "name": name, "title": assistants::title(name), "found": found, "how": how, "default": by_default,
        }));
    }
    if cli.json {
        print_json(&rows);
        return Ok(0);
    }
    for row in &rows {
        let text = |key: &str| row[key].as_str().unwrap_or_default().to_owned();
        let found = match row["found"].as_str() {
            Some(found) => format!("on this machine ({found})"),
            None => "not found here".to_owned(),
        };
        println!("{:<8} {:<12} {:<38} {}", text("name"), text("title"), found, text("how"));
    }
    match defaults {
        Some(list) if list.is_empty() => println!("New projects get no assistant (`kito assistant default`)."),
        Some(list) => println!("New projects get: {} (`kito assistant default`).", list.join(", ")),
        None => println!("New projects: `kito init` asks, for the ones on this machine (`kito assistant default` says it once)."),
    }
    println!("`kito assistant add <name>` sets one up in this repository.");
    Ok(0)
}

/// `kito assistant default`: says, or changes, which assistants every new
/// project gets.
fn assistant_default(cli: &Cli, names: &[String], none: bool) -> Result<u8, String> {
    if names.is_empty() && !none {
        let said = assistants::defaults();
        if cli.json {
            print_json(&json!({ "default": said }));
        } else {
            match said {
                Some(list) if list.is_empty() => println!("New projects get no assistant."),
                Some(list) => println!("New projects get: {}.", list.join(", ")),
                None => println!("Not said: `kito init` asks, for the assistants on this machine."),
            }
        }
        return Ok(0);
    }
    let path = assistants::set_defaults(names)?;
    if cli.json {
        print_json(&json!({ "default": names, "file": path }));
    } else if names.is_empty() {
        println!("New projects get no assistant ({}).", path.display());
    } else {
        let titles: Vec<String> = names.iter().map(|name| assistants::title(name)).collect();
        println!("New projects get: {} ({}).", titles.join(", "), path.display());
    }
    Ok(0)
}

/// Runs a command of a plugin this repository enabled, with what it needs
/// to reach the daemon in its environment. Returns its exit code.
fn cmd_external(cli: &Cli, line: &[OsString]) -> Result<u8, String> {
    use kitod::plugins;
    let asked = line.first().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let unknown = || format!("`{asked}` is not a command of Kito (`kito --help` lists them)");
    let root = root_of(cli).map_err(|_| unknown())?;
    let mut client = Client::connect_or_start(&root).map_err(fail)?;
    let listed: Value = client.call("plugins", json!({})).map_err(fail)?;
    let provides = |plugin: &&Value| {
        plugin["commands"].as_array().into_iter().flatten().any(|command| command["name"] == asked.as_str())
    };
    let all: &[Value] = listed.as_array().map_or(&[], Vec::as_slice);
    let Some(plugin) = all.iter().find(|plugin| plugin["enabled"] == true && provides(plugin)) else {
        // Enabled here and missing for this person: its commands are not
        // known, its name is.
        let missing = all.iter().find(|plugin| {
            plugin["enabled"] == true && plugin["installed"] == false && plugin["name"] == asked.as_str()
        });
        if let Some(problem) = missing.and_then(|plugin| plugin["problem"].as_str()) {
            return Err(problem.to_owned());
        }
        // Installed, and not let in here: say how.
        let has = plugins::all_installed()
            .into_iter()
            .find(|plugin| plugin.manifest.commands.iter().any(|command| command.name == asked));
        return Err(match has {
            Some(plugin) => format!(
                "`kito {asked}` is a command of the plugin `{}`, which this repository did not enable: \
                 `kito plugin enable {}`",
                plugin.manifest.name, plugin.manifest.name
            ),
            None => unknown(),
        });
    };
    run_plugin_here(&root, &mut client, plugin, &line[1..], false)
}

/// Where the review of a lane stands, as text.
fn review_text(review: &Value) -> String {
    use std::fmt::Write as _;
    let lane = review["lane"].as_str().unwrap_or_default();
    let mut out = String::new();
    let required = review["required"].as_u64().unwrap_or(0);
    let approved = strings(&review["approved_by"]);
    let _ = match (review["missing"].as_str(), approved.is_empty()) {
        (Some(missing), _) => writeln!(out, "{lane}: {missing}"),
        (None, false) => writeln!(
            out,
            "{lane}: approved by {}. `kito integrate {lane}` brings it in.",
            approved.join(", ")
        ),
        (None, true) => writeln!(out, "{lane}: no verdict yet, and none is needed"),
    };
    for verdict in review["verdicts"].as_array().into_iter().flatten() {
        let text = |key: &str| verdict[key].as_str().unwrap_or_default();
        let whose = match verdict["for"].as_str() {
            Some(person) => format!("{}, of {person}", text("by")),
            None => text("by").to_owned(),
        };
        let words = verdict["text"].as_str().map(|words| format!(": {words}")).unwrap_or_default();
        let not =
            verdict["because"].as_str().map(|why| format!("  [does not count: {why}]")).unwrap_or_default();
        let _ = writeln!(out, "  {} by {whose}{words}{not}", text("verdict").replace('_', " "));
    }
    if required > 0 {
        let _ = writeln!(out, "  approvals needed: {required}");
    }
    match review["comments"].as_u64().unwrap_or(0) {
        0 => {}
        1 => {
            let _ = writeln!(out, "  1 comment waits (`kito lane comments {lane}`)");
        }
        more => {
            let _ = writeln!(out, "  {more} comments wait (`kito lane comments {lane}`)");
        }
    }
    out
}

/// Comments as text: each with what it quoted, its answers under it.
fn comments_text(comments: &[Value]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for comment in comments {
        let text = |key: &str| comment[key].as_str().unwrap_or_default();
        let pad = if comment["reply_to"].is_string() { "  " } else { "" };
        let on = comment["on"].as_str().map(|on| format!(" on {on}")).unwrap_or_default();
        let whose = match comment["for"].as_str() {
            Some(person) => format!("{}, of {person}", text("by")),
            None => text("by").to_owned(),
        };
        let mut marks = String::new();
        if comment["outdated"] == true {
            marks.push_str("  [the code changed since]");
        }
        if let Some(by) = comment["resolved_by"].as_str() {
            let _ = write!(marks, "  [resolved by {by}]");
        }
        let _ = writeln!(out, "{pad}{}{on} ({whose}): {}{marks}", text("id"), text("text"));
        for line in text("quoted").lines().filter(|_| comment["quoted"].is_string()) {
            let _ = writeln!(out, "{pad}  > {line}");
        }
    }
    out
}

/// The diff of a lane as text: a line per file, its functions, its hunks.
fn lane_diff_text(diff: &Value, stat: bool) -> String {
    use std::fmt::Write as _;
    let text = |value: &Value, key: &str| value[key].as_str().unwrap_or_default().to_owned();
    let count = |value: &Value, key: &str| value[key].as_u64().unwrap_or(0);
    let files: &[Value] = diff["files"].as_array().map_or(&[], Vec::as_slice);
    let (added, removed) =
        files.iter().fold((0, 0), |(a, r), file| (a + count(file, "added"), r + count(file, "removed")));
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} onto {} (base {}): {} file{}, +{added} -{removed}",
        text(diff, "lane"),
        text(diff, "onto"),
        text(diff, "base"),
        files.len(),
        if files.len() == 1 { "" } else { "s" },
    );
    match count(diff, "behind") {
        0 => {}
        behind => {
            let _ = writeln!(
                out,
                "{} changed {behind} file{} since the lane started: it has not seen them",
                text(diff, "onto"),
                if behind == 1 { "" } else { "s" }
            );
        }
    }
    for file in files {
        let was = file["was"].as_str().map(|was| format!(" (was {was})")).unwrap_or_default();
        let lines = match file["binary"].as_bool() {
            Some(true) => "binary".to_owned(),
            _ => format!("+{} -{}", count(file, "added"), count(file, "removed")),
        };
        let _ = writeln!(out, "\n{:<8} {}{was}  {lines}", text(file, "change"), text(file, "path"));
        for symbol in file["symbols"].as_array().into_iter().flatten() {
            // The name without its file: the file is the line above.
            let name = text(symbol, "symbol");
            let name = name.rsplit_once(':').map_or(name.as_str(), |(_, name)| name).to_owned();
            let at = match (symbol["lines"][0].as_u64(), symbol["lines"][1].as_u64()) {
                (Some(from), Some(to)) if from == to => format!(" (line {from})"),
                (Some(from), Some(to)) => format!(" (lines {from}-{to})"),
                _ => String::new(),
            };
            let was = symbol["was"].as_str().map(|was| format!(", was {was}")).unwrap_or_default();
            let why: Vec<String> =
                strings(&symbol["intents"]).iter().map(|why| format!("\"{why}\"")).collect();
            let why = if why.is_empty() { String::new() } else { format!(": {}", why.join(", ")) };
            let _ = writeln!(out, "  {:<8} {name}{at}{was}{why}", text(symbol, "change"));
        }
        if stat {
            continue;
        }
        for hunk in file["hunks"].as_array().into_iter().flatten() {
            let side = |key: &str| {
                format!("{},{}", hunk[key][0].as_u64().unwrap_or(0), hunk[key][1].as_u64().unwrap_or(0))
            };
            let _ = writeln!(out, "@@ -{} +{} @@", side("before"), side("after"));
            for line in strings(&hunk["lines"]) {
                let _ = writeln!(out, "{line}");
            }
        }
    }
    out
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

/// What was said of the assistants a new project gets (ADR 0011).
struct Chosen {
    no_claude: bool,
    names: Vec<String>,
    none: bool,
}

/// The assistants a new project gets, and the ones found and left out.
/// Claude Code's files are written now, before the first scan records
/// them; the others are set up by their plugins once the daemon runs.
struct NewAssistants {
    wanted: Vec<String>,
    others: Vec<&'static assistants::Known>,
    claude: Option<claude::Setup>,
}

fn assistants_before(cli: &Cli, root: &Path, chosen: &Chosen) -> NewAssistants {
    let (wanted, others) =
        assistants::for_new_project(&chosen.names, chosen.none, chosen.no_claude, !cli.json);
    let claude = wanted.iter().any(|name| name == "claude").then(|| claude::setup(root));
    NewAssistants { wanted, others, claude }
}

/// Sets up the assistants other than Claude Code, says how each went, and
/// what else was found. A plugin that fails does not undo the project.
fn assistants_after(cli: &Cli, root: &Path, client: &mut Client, new: &NewAssistants) -> Value {
    let mut said = Vec::new();
    for name in new.wanted.iter().filter(|name| *name != "claude") {
        let done = add_by_plugin(root, client, name);
        if !cli.json {
            match &done {
                Ok(lines) => {
                    println!("Set up for {}:", assistants::title(name));
                    lines.iter().for_each(|line| println!("  {line}"));
                }
                Err(why) => println!(
                    "{} is not set up: {why} (`kito assistant add {name}` tries again)",
                    assistants::title(name)
                ),
            }
        }
        said.push(match done {
            Ok(lines) => json!({ "name": name, "set_up": true, "said": lines }),
            Err(why) => json!({ "name": name, "set_up": false, "error": why }),
        });
    }
    if !cli.json && !new.others.is_empty() {
        let titles: Vec<&str> = new.others.iter().map(|one| one.title).collect();
        let names: Vec<&str> = new.others.iter().map(|one| one.name).collect();
        println!(
            "Also on this machine: {}. `kito assistant add {}` sets {} up here.",
            titles.join(", "),
            names.join(" "),
            if names.len() == 1 { "it" } else { "them" }
        );
    }
    let written = instructions::write(root, client).unwrap_or_else(|why| vec![why]);
    if !cli.json {
        written.iter().for_each(|line| println!("Instructions: {line}"));
    }
    json!({
        "set_up": new.wanted,
        "instructions": written,
        "plugins": said,
        "also_found": new.others.iter().map(|one| one.name).collect::<Vec<_>>(),
    })
}

/// Sets up an assistant that a plugin knows: the plugin enabled here, or
/// the one installed that knows it, enabled now, or the one of that name
/// in the registry, installed and enabled now. Answers what was done.
fn add_by_plugin(root: &Path, client: &mut Client, name: &str) -> Result<Vec<String>, String> {
    use kitod::plugins;
    let mut said = Vec::new();
    let enabled_here = |client: &mut Client| -> Result<Option<Value>, String> {
        let listed: Value = client.call("plugins", json!({})).map_err(fail)?;
        Ok(listed
            .as_array()
            .into_iter()
            .flatten()
            .find(|plugin| plugin["enabled"] == true && plugin["assistant"] == name)
            .cloned())
    };
    let plugin = match enabled_here(client)? {
        Some(plugin) => plugin,
        None => {
            let installed = plugins::all_installed().into_iter().find(|plugin| {
                plugin.manifest.assistant.as_ref().is_some_and(|assistant| assistant.name == name)
            });
            let plugin_name = match installed {
                Some(plugin) => plugin.manifest.name,
                None => {
                    let wanted = assistants::known(name).and_then(|one| one.plugin).unwrap_or(name);
                    let (added, source) = kitod::registry::install(wanted, None)
                        .map_err(|why| format!("its plugin `{wanted}` could not be installed: {why}"))?;
                    said.push(format!(
                        "installed the plugin {} {} from {}",
                        added.manifest.name, added.manifest.version, source.registry
                    ));
                    added.manifest.name
                }
            };
            let enabled: Value = client
                .call("plugin_enable", json!({ "name": plugin_name, "permissions": Value::Null }))
                .map_err(fail)?;
            let given = strings(&enabled["permissions"]);
            let given = if given.is_empty() { "nothing".to_owned() } else { given.join(", ") };
            said.push(format!("enabled the plugin {plugin_name} here: it may {given}"));
            enabled_here(client)?
                .ok_or_else(|| format!("the plugin {plugin_name} does not set `{name}` up"))?
        }
    };
    let plugin_name = plugin["name"].as_str().unwrap_or_default().to_owned();
    let program = plugin["program"].as_str().map(PathBuf::from).ok_or("the plugin's program is not there")?;
    let key: Value = client.call("plugin_key", json!({ "name": plugin_name })).map_err(fail)?;
    let printed = plugins::run_plugin(
        &program,
        &["assistant", "setup"],
        root,
        key["key"].as_str().unwrap_or_default(),
        &[],
        std::time::Duration::from_secs(120),
    )?;
    said.extend(
        String::from_utf8_lossy(&printed).lines().map(str::to_owned).filter(|line| !line.trim().is_empty()),
    );
    Ok(said)
}

/// The files assistants read, written again after one was set up or taken out.
fn instructions_written(cli: &Cli) {
    let Ok(root) = root_of(cli) else { return };
    let Ok(mut client) = Client::connect_or_start(&root) else { return };
    match instructions::write(&root, &mut client) {
        Ok(lines) if !cli.json => lines.iter().for_each(|line| println!("Instructions: {line}")),
        Ok(_) => {}
        Err(why) => eprintln!("kito: the instructions were not written: {why}"),
    }
}

/// `kito instructions` (ADR 0012).
fn cmd_instructions(cli: &Cli, command: Option<&InstructionsCmd>) -> Outcome {
    let root = root_of(cli)?;
    let mut client = daemon(cli)?;
    let write_now = |client: &mut Client| -> Outcome {
        let lines = instructions::write(&root, client)?;
        if !cli.json {
            lines.iter().for_each(|line| println!("  {line}"));
        }
        Ok(())
    };
    match command.unwrap_or(&InstructionsCmd::List) {
        InstructionsCmd::List => {
            let got: Value = client.call("instructions", json!({})).map_err(fail)?;
            if cli.json {
                print_json(&got);
                return Ok(());
            }
            match got["kitchen"].as_str() {
                Some(kitchen) if got["kitchen_found"] == true => println!("Kitchen: {kitchen}"),
                Some(kitchen) => {
                    println!("Kitchen: {kitchen}, which is not there: only this repository's own")
                }
                None => println!("Kitchen: the standard one (Kito's rules alone)"),
            }
            println!("Every assistant is told Kito's rules first, then:");
            let items = got["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("  nothing more (`kito instructions set <name>` adds to it)");
            }
            for item in &items {
                let name = item["name"].as_str().unwrap_or_default();
                let from = match (item["from"].as_str(), item["overrides"] == true) {
                    (Some("repository"), true) => "this repository, over the kitchen's",
                    (Some("repository"), false) => "this repository",
                    _ => "the kitchen",
                };
                let text = item["text"].as_str().unwrap_or_default();
                let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
                println!("  {name:<24} {from}: {}", first.trim().trim_start_matches('#').trim());
            }
            for name in strings(&got["removed"]) {
                println!("  {name:<24} the kitchen's, not told here");
            }
            Ok(())
        }
        InstructionsCmd::Show { assistant } => {
            let got: Value = client.call("instructions", json!({})).map_err(fail)?;
            let folders: Vec<&str> = vec!["all", assistant.as_str()];
            let text = instructions::compose(got["items"].as_array().map_or(&[], Vec::as_slice), &folders);
            match cli.json {
                true => print_json(&json!({ "assistant": assistant, "text": text })),
                false => print!("{text}"),
            }
            Ok(())
        }
        InstructionsCmd::Set { name, file } => {
            let text = match file {
                Some(file) => {
                    std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?
                }
                None => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                        .map_err(|e| e.to_string())?;
                    text
                }
            };
            let done: Value =
                client.call("instructions_set", json!({ "name": name, "text": text })).map_err(fail)?;
            let name = done["name"].as_str().unwrap_or(name);
            if !cli.json {
                match (done["changed"] == true, text.trim().is_empty()) {
                    (false, _) => println!("{name} says that already."),
                    (true, true) => println!("{name} of the kitchen is not told here any more."),
                    (true, false) => {
                        println!("{name} is set: every assistant here gets it at its next session.")
                    }
                }
            }
            write_now(&mut client)?;
            if cli.json {
                print_json(&done);
            }
            Ok(())
        }
        InstructionsCmd::Remove { name } => {
            let done: Value = client.call("instructions_remove", json!({ "name": name })).map_err(fail)?;
            let name = done["name"].as_str().unwrap_or(name);
            if !cli.json {
                match done["changed"] == true {
                    true => println!("{name} is removed."),
                    false => println!("This repository has no instruction {name} of its own."),
                }
            }
            write_now(&mut client)?;
            if cli.json {
                print_json(&done);
            }
            Ok(())
        }
        InstructionsCmd::Kitchen { dir, none } => {
            let kitchen = match (dir, none) {
                (Some(dir), false) => {
                    let dir = std::path::absolute(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                    if !dir.join(kitod::instructions::DIR).is_dir() {
                        return Err(format!(
                            "{} has no `instructions/` folder: a kitchen keeps them there, as `all/<name>.md` \
                             and `<assistant>/<name>.md`",
                            dir.display()
                        ));
                    }
                    json!(dir.display().to_string())
                }
                _ => Value::Null,
            };
            let now: Value = client.call("configure", json!({ "kitchen": kitchen })).map_err(fail)?;
            if !cli.json {
                match now["kitchen"].as_str() {
                    Some(kitchen) => {
                        println!("The kitchen is {kitchen}: its instructions come before this repository's.")
                    }
                    None => println!(
                        "The standard kitchen: Kito's rules, then this repository's own instructions."
                    ),
                }
            }
            write_now(&mut client)?;
            if cli.json {
                print_json(&now);
            }
            Ok(())
        }
        InstructionsCmd::Write => {
            let lines = instructions::write(&root, &mut client)?;
            match cli.json {
                true => print_json(&lines),
                false if lines.is_empty() => println!("The files assistants read here say it already."),
                false => lines.iter().for_each(|line| println!("{line}")),
            }
            Ok(())
        }
    }
}

/// Claude Code set up in this repository, by Kito itself.
fn claude_here(cli: &Cli, root: &Path) -> Result<claude::Setup, String> {
    // On a protected main lane the files would be put back as soon as
    // they are written: said before, not found out after.
    // Its files are kept out of the history (ADR 0012): nothing to put back.
    let recorded =
        kitod::config::Config::load(&root.join(".kito")).is_ok_and(|config| config.agents.files_in_history);
    if Remote::load(root).is_none() && recorded {
        let status: DaemonStatus = daemon(cli)?.call("status", json!({})).map_err(fail)?;
        if status.repo.protected {
            return Err("the main lane is protected, and the set-up writes files in the main directory: \
                        `kito protect --off`, `kito assistant add claude`, `kito checkpoint -m \"set up for Claude \
                        Code\"`, then `kito protect`"
                .into());
        }
    }
    Ok(claude::setup(root))
}

fn cmd_claude(cli: &Cli) -> Outcome {
    let root = root_of(cli)?;
    let setup = claude_here(cli, &root)?;
    if cli.json {
        print_json(&setup.json());
    } else {
        setup.print();
    }
    instructions_written(cli);
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

fn cmd_init(cli: &Cli, path: &Option<PathBuf>, fresh: bool, chosen: &Chosen) -> Outcome {
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
    let new = assistants_before(cli, &root, chosen);
    // Capture is continuous from the very start: the daemon starts with the repository.
    let mut client = Client::connect_or_start(&root).map_err(fail)?;
    if !cli.json {
        println!("Kito repository created in {}", root.join(".kito").display());
        if let Some(setup) = &new.claude {
            setup.print();
        }
    }
    let assistants = assistants_after(cli, &root, &mut client, &new);
    if cli.json {
        print_json(&json!({
            "root": root, "claude": new.claude.as_ref().map(claude::Setup::json), "assistants": assistants,
        }));
    } else {
        println!("The daemon is running: every save is recorded from now on.");
        warn_if_events_unreliable(&mut client);
    }
    Ok(())
}

fn cmd_import(cli: &Cli, path: &Option<PathBuf>, chosen: &Chosen) -> Outcome {
    let root = path.clone().or_else(|| cli.repo.clone()).unwrap_or_else(|| PathBuf::from("."));
    let root = root.canonical().map_err(|e| format!("{}: {e}", root.display()))?;
    not_connected(&root)?;
    // The import works directly on the store: the daemon, if there is one, steps aside.
    stop_daemon(&root)?;
    let mut repo =
        if root.join(".kito").join("format").is_file() { Repo::open(&root) } else { Repo::init(&root) }
            .map_err(fail)?;
    let report = repo.import_git().map_err(fail)?;
    let new = assistants_before(cli, &root, chosen);
    let setup = &new.claude;
    let captured = repo.scan().map_err(fail)?;
    repo.close().map_err(fail)?;
    let mut client = Client::connect_or_start(&root).map_err(fail)?;
    let assistants = match cli.json {
        true => assistants_after(cli, &root, &mut client, &new),
        false => Value::Null,
    };
    if cli.json {
        print_json(&json!({
            "commits": report.commits,
            "ops": report.ops,
            "checkpoints": report.checkpoints,
            "skipped": report.skipped,
            "uncommitted_ops": captured.len(),
            "claude": setup.as_ref().map(claude::Setup::json),
            "assistants": assistants,
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
        if let Some(setup) = setup {
            setup.print();
        }
        assistants_after(cli, &root, &mut client, &new);
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
        let reviewers = strings(&reason["reviewed_by"]);
        let reviewed = (!reviewers.is_empty()).then(|| format!("reviewed by {}", reviewers.join(", ")));
        let passed = (!checks.is_empty()).then(|| format!("checks passed: {}", checks.join("; ")));
        let said: Vec<String> = passed.into_iter().chain(reviewed).chain(approved).collect();
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
    if let Some(review) = &lane.review {
        line.push_str(&format!("  [{review}]"));
    }
    if let Some(assistant) = &lane.assistant {
        match &assistant.model {
            Some(model) => line.push_str(&format!("  in {} ({model})", assistant.name)),
            None => line.push_str(&format!("  in {}", assistant.name)),
        }
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
fn remote_for(
    server: &str,
    repo: &str,
    token: &Option<String>,
    sign_in: &Option<String>,
) -> Result<Remote, String> {
    if let Some(rest) = server.strip_prefix("ssh://") {
        if sign_in.is_some() {
            return Err(
                "a server reached through SSH knows you by your key: --sign-in is for host:port".into()
            );
        }
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
    if sign_in.is_some() {
        // The token comes when they have signed in, after the server is
        // known to be the one meant.
        return Ok(Remote {
            server: Some(server.to_owned()),
            repo: repo.to_owned(),
            sign_in: sign_in.clone(),
            ..Remote::default()
        });
    }
    // None: asked of the server once it is known to be the one meant
    // (`signed_in`).
    let token =
        token.clone().or_else(|| std::env::var("KITO_TOKEN").ok()).filter(|token| !token.trim().is_empty());
    Ok(Remote { server: Some(server.to_owned()), repo: repo.to_owned(), token, ..Remote::default() })
}

/// The port of a Kito server when an address says none.
const DEFAULT_PORT: u16 = 7421;

/// What `kito connect` was given, as the server, the repository and the
/// directory: one address (`kito.example.com/shop`,
/// `kito://host:port/shop`), or the server and then the repository.
fn address_of(
    given: &str,
    second: Option<&str>,
    dir: Option<PathBuf>,
) -> Result<(String, String, Option<PathBuf>), String> {
    let plain = given.strip_prefix("kito://").unwrap_or(given);
    let one_address = !given.starts_with("ssh://") && plain.contains('/');
    if !one_address {
        let repo = second.ok_or_else(|| {
            format!("say the repository too: `kito connect {given}/<repository>`, or `kito connect {given} <repository>`")
        })?;
        return Ok((given.to_owned(), repo.to_owned(), dir));
    }
    if given.starts_with("http://") || given.starts_with("https://") {
        return Err(format!(
            "{given} is the address of a web page: give the server's own, as `kito connect host[:port]/<repository>`"
        ));
    }
    if dir.is_some() {
        return Err("an address names the repository already: give only the directory after it".into());
    }
    let (host, repo) = plain.trim_end_matches('/').split_once('/').unwrap_or((plain, ""));
    if host.is_empty() || repo.is_empty() || repo.contains('/') {
        return Err(format!("{given}: an address is host[:port]/<repository>"));
    }
    // A port said, or not: `[::1]:7421`, `host:7421`, `host`.
    let has_port = host.rsplit_once(':').is_some_and(|(before, port)| {
        port.parse::<u16>().is_ok() && (!before.contains(':') || before.ends_with(']'))
    });
    let server = match has_port {
        true => host.to_owned(),
        false => format!("{host}:{DEFAULT_PORT}"),
    };
    Ok((server, repo.to_owned(), second.map(PathBuf::from)))
}

/// Opens a page in the person's browser, when somebody is there to see it.
/// Answers whether it was opened.
fn open_in_browser(url: &str) -> bool {
    if std::env::var_os("KITO_NO_BROWSER").is_some_and(|value| !value.is_empty()) || !assistants::can_ask() {
        return false;
    }
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return false;
    }
    let quiet = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if let Some(browser) = std::env::var_os("BROWSER").filter(|browser| !browser.is_empty()) {
        return quiet(&browser.to_string_lossy(), &[url]);
    }
    if cfg!(target_os = "macos") {
        return quiet("open", &[url]);
    }
    if cfg!(windows) {
        return quiet("cmd", &["/C", "start", "", url]);
    }
    let wsl =
        std::fs::read_to_string("/proc/version").is_ok_and(|text| text.to_lowercase().contains("microsoft"));
    (wsl && (quiet("wslview", &[url]) || quiet("cmd.exe", &["/C", "start", "", url])))
        || quiet("xdg-open", &[url])
}

/// Reads what is typed at the terminal without showing it.
fn read_secret(prompt: &str) -> Result<String, String> {
    use std::io::Write as _;
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    #[cfg(unix)]
    let tty = std::fs::File::open("/dev/tty").ok();
    #[cfg(unix)]
    let stty = |arg: &str| {
        if let Some(tty) = tty.as_ref().and_then(|tty| tty.try_clone().ok()) {
            let _ = std::process::Command::new("stty").arg(arg).stdin(tty).status();
        }
    };
    #[cfg(unix)]
    stty("-echo");
    let mut line = String::new();
    let read = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line);
    #[cfg(unix)]
    stty("echo");
    eprintln!();
    read.map_err(|e| e.to_string())?;
    Ok(line.trim().to_owned())
}

/// The remote with who the person is: the token given, a way to sign in
/// the server offers (the only one, or the one chosen at the terminal),
/// or a token pasted at the terminal.
fn signed_in(remote: Remote, sign_in: &Option<String>) -> Result<Remote, String> {
    if let Some(method) = sign_in {
        return sign_in_to(remote, method).map(|(remote, _)| remote);
    }
    if remote.server.is_none() || remote.token.is_some() {
        return Ok(remote);
    }
    let ways = Client::sign_in_methods(&remote).map_err(fail)?;
    let asking = assistants::can_ask();
    let paste = |mut remote: Remote| -> Result<Remote, String> {
        let token = read_secret("Paste the token the server gave you (`kito-server user add` prints it): ")?;
        if token.is_empty() {
            return Err("no token was given: nothing is connected".into());
        }
        remote.token = Some(token);
        Ok(remote)
    };
    match ways.as_slice() {
        [] if asking => paste(remote),
        [] => Err(format!(
            "{} wants a token (--token, or $KITO_TOKEN): it offers no way to sign in in a browser",
            remote.place()
        )),
        [(method, _)] if !asking => sign_in_to(remote, method).map(|(remote, _)| remote),
        _ if !asking => {
            let names: Vec<&str> = ways.iter().map(|(method, _)| method.as_str()).collect();
            Err(format!(
                "{} offers more than one way to sign in: say which with --sign-in ({}), or give --token",
                remote.place(),
                names.join(", ")
            ))
        }
        _ => {
            eprintln!("How do you want to sign in to {}?", remote.place());
            for (n, (method, about)) in ways.iter().enumerate() {
                let about = if about.is_empty() { format!("with {method}") } else { about.clone() };
                eprintln!("  {}. {about}, in your browser", n + 1);
            }
            eprintln!("  {}. Paste a token", ways.len() + 1);
            eprint!("[1] ");
            let mut answer = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer)
                .map_err(|e| e.to_string())?;
            let chosen = match answer.trim() {
                "" => 1,
                number => number.parse::<usize>().map_err(|_| format!("`{number}` is none of them"))?,
            };
            match chosen {
                n if (1..=ways.len()).contains(&n) => {
                    sign_in_to(remote, &ways[n - 1].0).map(|(remote, _)| remote)
                }
                n if n == ways.len() + 1 => paste(remote),
                n => Err(format!("`{n}` is none of them")),
            }
        }
    }
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
    // Vouched for by a public authority, for its name: known by it.
    let vouched = std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(10))
        .is_ok_and(|stream| {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
            kitod::tls::vouched(stream, &host)
        });
    if vouched {
        remote.authority = true;
        return Ok(remote);
    }
    // Asked once, as ssh does, and kept from then on.
    if assistants::can_ask() {
        eprintln!(
            "{} has a certificate of its own, with fingerprint\n  {presented}\nCompare it with what              `kito-server fingerprint` prints on the server.",
            remote.place()
        );
        eprint!("Is it the server you mean? [y/N] ");
        let mut answer = String::new();
        let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer);
        if matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            remote.fingerprint = Some(presented);
            return Ok(remote);
        }
        return Err("nothing was sent to it, and nothing is connected".into());
    }
    Err(format!(
        "{} is on another machine, and nothing says it is the server you mean. It presents a certificate \
         with fingerprint\n  {presented}\nIf that is what `kito-server fingerprint` prints on the server, \
         connect again with\n  --fingerprint {presented}\n(or with --authority if its certificate comes \
         from a public authority).",
        remote.place()
    ))
}

/// Signs the person in to the remote's server and returns the remote with
/// the session's token, and what the server said. Where to go and what to
/// enter is said on standard error: what a command prints stays its own.
fn sign_in_to(mut remote: Remote, method: &str) -> Result<(Remote, Value), String> {
    let mut tell = |how: &Value| {
        let page = how["open"].as_str().unwrap_or("?");
        eprintln!("To sign in with {method}, open {page}");
        if let Some(code) = how["code"].as_str() {
            eprintln!("and enter the code {code}");
        }
        if open_in_browser(page) {
            eprintln!("(opened in your browser)");
        }
        eprintln!("Waiting until you have...");
    };
    let signed = Client::sign_in(&remote, method, &mut tell).map_err(fail)?;
    let token = signed["token"].as_str().ok_or("the server opened no session")?;
    remote.token = Some(token.to_owned());
    remote.sign_in = Some(method.to_owned());
    Ok((remote, signed))
}

fn cmd_sign_in(cli: &Cli, method: &Option<String>) -> Outcome {
    let root = root_of(cli)?;
    let remote = Remote::load(&root).ok_or(
        "this directory is a repository of its own: `kito sign-in` is for one connected to a server",
    )?;
    if remote.server.is_none() {
        return Err("this directory reaches its server through SSH, which knows you by your key".into());
    }
    let method = method.clone().or_else(|| remote.sign_in.clone()).ok_or(
        "this directory was connected with a token: say the way to sign in, as in `kito sign-in github`",
    )?;
    let (remote, signed) = sign_in_to(remote, &method)?;
    // Asked as the person they now are, before anything is kept.
    Client::remote(&remote, true).map_err(fail)?;
    remote.save(&root).map_err(fail)?;
    // The directory's daemon holds the session it started with: the next
    // command starts one that has this one.
    if let Ok(mut client) = Client::connect(&root) {
        let _ = client.call_value("shutdown", json!({}));
    }
    let (user, days) = (signed["user"].as_str().unwrap_or("?"), signed["days"].as_u64().unwrap_or(0));
    if cli.json {
        print_json(&json!({ "user": user, "method": method, "days": days, "server": remote.place() }));
    } else {
        println!(
            "Signed in to {} as {user}: the session lasts {}.",
            remote.place(),
            count(days, "day", "days")
        );
    }
    Ok(())
}

fn cmd_connect(cli: &Cli, remote: Remote, repo: &str, dir: &Option<PathBuf>, chosen: &Chosen) -> Outcome {
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
    let new = assistants_before(cli, &dir, chosen);
    let setup = &new.claude;
    let assistants = match Client::connect_or_start(&dir) {
        Ok(mut client) if cli.json => assistants_after(cli, &dir, &mut client, &new),
        _ => Value::Null,
    };
    if cli.json {
        print_json(&json!({
            "assistants": assistants,
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
        if let Ok(mut client) = Client::connect_or_start(&dir) {
            assistants_after(cli, &dir, &mut client, &new);
        }
    }
    Ok(())
}

/// `7d`, `12h`, `2w` before now, or a day (`2026-11-01`), in milliseconds since 1970.
fn moment(given: &str) -> Result<u64, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    let given = given.trim();
    let unit = |c: char| match c {
        'h' => Some(3_600_000u64),
        'd' => Some(86_400_000),
        'w' => Some(7 * 86_400_000),
        _ => None,
    };
    if let Some(last) = given.chars().last() {
        if let (Some(size), Ok(n)) = (unit(last), given[..given.len() - 1].parse::<u64>()) {
            return Ok(now.saturating_sub(n * size));
        }
    }
    let parts: Vec<u64> = given.split('-').filter_map(|part| part.parse().ok()).collect();
    if let [year, month, day] = parts[..] {
        if (1..=12).contains(&month) && (1..=31).contains(&day) {
            // Days since 1970 of a date, by the civil calendar.
            let (y, m) = if month <= 2 { (year - 1, month + 9) } else { (year, month - 3) };
            let era = y / 400;
            let yoe = y - era * 400;
            let doy = (153 * m + 2) / 5 + day - 1;
            let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
            let days = era * 146_097 + doe - 719_468;
            return Ok(days * 86_400_000);
        }
    }
    Err(format!("`{given}` is no time: say 7d, 12h, 2w, or a day as 2026-11-01"))
}

/// The repositories a question goes to: this one, or with `everywhere`
/// every one the person may use on the server this directory is connected to.
fn asked_everywhere(
    cli: &Cli,
    method: &str,
    params: &Value,
    everywhere: bool,
) -> Result<Vec<(String, Value)>, String> {
    if !everywhere {
        let answer: Value = daemon(cli)?.call(method, params.clone()).map_err(fail)?;
        return Ok(vec![(String::new(), answer)]);
    }
    let root = root_of(cli)?;
    let remote = Remote::load(&root).ok_or(
        "--everywhere is for a directory connected to a server: it asks every repository you may use there",
    )?;
    let repos: Vec<String> =
        Client::remote(&remote, false).map_err(fail)?.call("server.repos", json!({})).map_err(fail)?;
    let mut answers = Vec::new();
    for repo in repos {
        let there = Remote { repo: repo.clone(), ..remote.clone() };
        match Client::remote(&there, true).and_then(|mut client| client.call::<Value>(method, params.clone()))
        {
            Ok(answer) => answers.push((repo, answer)),
            Err(e) => eprintln!("kito: {repo}: {e}"),
        }
    }
    Ok(answers)
}

#[allow(clippy::too_many_arguments)]
fn cmd_search(
    cli: &Cli,
    words: &[String],
    kinds: &[String],
    since: &Option<String>,
    until: &Option<String>,
    who: &Option<String>,
    place: &Option<String>,
    outcome: &Option<String>,
    limit: usize,
    everywhere: bool,
) -> Outcome {
    let mut params = json!({ "q": words.join(" "), "kinds": kinds, "limit": limit });
    if let Some(since) = since {
        params["since_ms"] = json!(moment(since)?);
    }
    if let Some(until) = until {
        params["until_ms"] = json!(moment(until)?);
    }
    for (key, value) in [("who", who), ("where", place), ("outcome", outcome)] {
        if let Some(value) = value {
            params[key] = json!(value);
        }
    }
    let mut found: Vec<Value> = Vec::new();
    for (repo, answer) in asked_everywhere(cli, "search", &params, everywhere)? {
        for mut hit in answer.as_array().cloned().unwrap_or_default() {
            if !repo.is_empty() {
                hit["repo"] = json!(repo);
            }
            found.push(hit);
        }
    }
    if everywhere {
        found.sort_by_key(|hit| std::cmp::Reverse(hit["at_ms"].as_u64().unwrap_or(0)));
        found.truncate(limit);
    }
    if cli.json {
        print_json(&found);
        return Ok(());
    }
    if found.is_empty() {
        println!("Nothing found.");
    }
    for hit in &found {
        let text = |key: &str| hit[key].as_str().unwrap_or_default();
        let id = [text("id"), text("lane"), text("checkpoint")]
            .into_iter()
            .find(|id| !id.is_empty())
            .unwrap_or("");
        let when = hit["at_ms"]
            .as_u64()
            .map(utc)
            .map(|at| at.chars().take(10).collect::<String>())
            .unwrap_or_else(|| "          ".to_owned());
        let by = match &hit["by"] {
            Value::Array(all) => all.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
            other => other.as_str().unwrap_or_default().to_owned(),
        };
        let repo = if text("repo").is_empty() { String::new() } else { format!("{} · ", text("repo")) };
        let outcome = match &hit["outcome"] {
            Value::Array(all) => {
                format!("  [{}]", all.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
            }
            Value::String(one) => format!("  [{one}]"),
            _ => String::new(),
        };
        println!(
            "{:<8} {repo}{:<12} {when}  {by}: {}{outcome}",
            text("kind"),
            id,
            text("title").lines().next().unwrap_or("")
        );
    }
    Ok(())
}

fn cmd_stats(cli: &Cli, since: &Option<String>, until: &Option<String>, everywhere: bool) -> Outcome {
    let mut params = json!({});
    if let Some(since) = since {
        params["since_ms"] = json!(moment(since)?);
    }
    if let Some(until) = until {
        params["until_ms"] = json!(moment(until)?);
    }
    let answers = asked_everywhere(cli, "stats", &params, everywhere)?;
    if cli.json {
        let all: serde_json::Map<String, Value> = answers
            .into_iter()
            .map(|(repo, answer)| (if repo.is_empty() { "this".to_owned() } else { repo }, answer))
            .collect();
        print_json(&Value::Object(all));
        return Ok(());
    }
    let span = |ms: &Value| ms.as_u64().map(seconds).unwrap_or_else(|| "—".to_owned());
    for (repo, stats) in &answers {
        if !repo.is_empty() {
            println!("== {repo}");
        }
        let flow = &stats["flow"];
        let weeks = flow["weeks"].as_array().cloned().unwrap_or_default();
        let sum = |key: &str| weeks.iter().map(|week| week[key].as_u64().unwrap_or(0)).sum::<u64>();
        println!(
            "Lanes: {} opened, {} integrated, {} given up; {} open now.",
            sum("opened"),
            sum("integrated"),
            sum("given_up"),
            flow["open_now"]
        );
        println!(
            "From open to integrated: {} (median); waiting to go in: {}; to a first review: {}.",
            span(&flow["median_open_to_integrated_ms"]),
            span(&flow["median_waiting_ms"]),
            span(&flow["median_to_first_review_ms"])
        );
        if let Some(from) = stats["kept_from_ms"].as_u64() {
            println!("(Counted from {}, when Kito began keeping what happened.)", utc(from));
        }
        println!("\nBy assistant and model:");
        for row in stats["assistants"].as_array().into_iter().flatten() {
            let model = row["model"].as_str().map(|model| format!(" · {model}")).unwrap_or_default();
            println!(
                "  {}{model}: {} lanes, {} integrated ({:.0}%), {} conflicts, {} refused, {} held back, {} failed checks",
                row["assistant"].as_str().unwrap_or("?"),
                row["lanes"],
                row["integrated"],
                row["integration_rate"].as_f64().unwrap_or(0.0) * 100.0,
                row["conflicts"],
                row["refused"],
                row["held"],
                row["failed_checks"]
            );
        }
        println!("\nWhere the code is busy:");
        for file in stats["hotspots"]["files"].as_array().into_iter().flatten().take(8) {
            println!(
                "  {:<40} {} lanes, {} conflicts",
                file["path"].as_str().unwrap_or(""),
                file["lanes"],
                file["conflicts"]
            );
        }
        println!("\nReview:");
        for row in stats["review"].as_array().into_iter().flatten() {
            println!(
                "  {}: {} approved, {} asked for changes, first look after {}",
                row["who"].as_str().unwrap_or("?"),
                row["approved"],
                row["changes_requested"],
                span(&row["median_ms"])
            );
        }
        println!();
    }
    Ok(())
}

/// A task as a line of a list: `task#3  in a lane  Prices in cents  (for bob; lane/2)`.
fn task_line(task: &Value) -> String {
    let state = match task["state"].as_str().unwrap_or("open") {
        "in_lane" => "in a lane",
        "ready" => "ready to close",
        other => other,
    };
    let mut more = Vec::new();
    if let Some(who) = task["for"].as_str() {
        more.push(format!("for {who}"));
    }
    if let Some(agent) = task["claimed_by"].as_str() {
        more.push(format!("taken up by {agent}"));
    }
    let lanes: Vec<&str> =
        task["lanes"].as_array().into_iter().flatten().filter_map(|lane| lane["id"].as_str()).collect();
    if !lanes.is_empty() {
        more.push(lanes.join(", "));
    }
    if let Some(agent) = task["thinks_done"].as_str() {
        more.push(format!("{agent} thinks it is done"));
    }
    let more = if more.is_empty() { String::new() } else { format!("  ({})", more.join("; ")) };
    format!(
        "{:<8} {:<15} {}{more}",
        task["id"].as_str().unwrap_or("?"),
        state,
        task["title"].as_str().unwrap_or("")
    )
}

/// `kito task` (ADR 0015).
fn cmd_task(cli: &Cli, command: Option<&TaskCmd>) -> Outcome {
    let mut client = daemon(cli)?;
    let answer: Value = match command.unwrap_or(&TaskCmd::List { all: false, assignee: None }) {
        TaskCmd::List { all, assignee } => {
            let mut params = json!({ "state": if *all { "all" } else { "open" } });
            if let Some(who) = assignee {
                params["for"] = json!(who);
            }
            let listed: Value = client.call("tasks", params).map_err(fail)?;
            if cli.json {
                print_json(&listed);
            } else if listed.as_array().is_none_or(Vec::is_empty) {
                println!(
                    "No task{}. `kito task add <title>` asks for one.",
                    if *all { "" } else { " to do" }
                );
            } else {
                listed.as_array().into_iter().flatten().for_each(|task| println!("{}", task_line(task)));
            }
            return Ok(());
        }
        TaskCmd::Show { task } => client.call("task", json!({ "task": task })).map_err(fail)?,
        TaskCmd::Add { title, text, file, assignee } => {
            let text = match (text, file) {
                (Some(text), _) => text.clone(),
                (None, Some(file)) => {
                    std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?
                }
                (None, None) => String::new(),
            };
            client
                .call("task_add", json!({ "title": title.join(" "), "text": text, "for": assignee }))
                .map_err(fail)?
        }
        TaskCmd::Comment { task, text } => {
            client.call("task_comment", json!({ "task": task, "text": text.join(" ") })).map_err(fail)?
        }
        TaskCmd::Assign { task, who } => client
            .call("task_update", json!({ "task": task, "for": who.clone().unwrap_or_default() }))
            .map_err(fail)?,
        TaskCmd::Done { task } => client.call("task_done", json!({ "task": task })).map_err(fail)?,
        TaskCmd::Reopen { task } => client.call("task_reopen", json!({ "task": task })).map_err(fail)?,
        TaskCmd::Drop { task } => client.call("task_drop", json!({ "task": task })).map_err(fail)?,
    };
    if cli.json {
        print_json(&answer);
        return Ok(());
    }
    println!("{}", task_line(&answer));
    if let Some(TaskCmd::Show { .. }) = command {
        println!(
            "Asked for by {}, {}.",
            answer["by"].as_str().unwrap_or("?"),
            utc(answer["at_ms"].as_u64().unwrap_or(0))
        );
        if let Some(text) = answer["text"].as_str().filter(|text| !text.is_empty()) {
            println!("\n{text}");
        }
        for comment in answer["comments"].as_array().into_iter().flatten() {
            println!(
                "\n{}, {}:\n  {}",
                comment["by"].as_str().unwrap_or("?"),
                utc(comment["at_ms"].as_u64().unwrap_or(0)),
                comment["text"].as_str().unwrap_or("").replace('\n', "\n  ")
            );
        }
        if answer["state"] == "ready" {
            println!(
                "\nIts lanes are closed: `kito task done {}` if it is done.",
                answer["id"].as_str().unwrap_or("")
            );
        } else if answer["state"] == "open" {
            println!(
                "\nWork on it in a lane: `kito lane open <agent> \"<why>\" --task {} ...`.",
                answer["id"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}

/// The server this directory is connected to, asked as its person: about
/// the repository (`repo`), or about the server.
fn server_here(cli: &Cli, repo: bool) -> Result<(Remote, Client), String> {
    let root = root_of(cli)?;
    let remote = Remote::load(&root)
        .ok_or("this directory is a repository of its own: who may use it is for a server to say")?;
    let client = Client::remote(&remote, repo).map_err(fail)?;
    Ok((remote, client))
}

/// `kito members` (ADR 0014).
fn cmd_members(cli: &Cli) -> Outcome {
    let (remote, mut client) = server_here(cli, true)?;
    let said: Value = client.call("server.members", json!({})).map_err(fail)?;
    if cli.json {
        print_json(&said);
        return Ok(());
    }
    println!("Who may use `{}` on {}:", remote.repo, remote.place());
    for member in said["members"].as_array().into_iter().flatten() {
        let by = if member["admin"] == true { "  (an admin of the server)" } else { "" };
        println!(
            "  {:<16} {}{by}",
            member["user"].as_str().unwrap_or("?"),
            member["role"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

/// `kito grant <name> <role>` and `kito revoke <name>`.
fn cmd_grant(cli: &Cli, name: &str, role: Option<&String>) -> Outcome {
    let (remote, mut client) = server_here(cli, true)?;
    let said: Value = client.call("server.grant", json!({ "user": name, "role": role })).map_err(fail)?;
    if cli.json {
        print_json(&said);
        return Ok(());
    }
    match (role, said["changed"] == true) {
        (Some(role), true) => println!("{name} may {role} in `{}` from their next connection.", remote.repo),
        (Some(role), false) => println!("{name} may {role} in `{}` already.", remote.repo),
        (None, true) => println!("{name} may no longer use `{}`.", remote.repo),
        (None, false) => println!("{name} was not given `{}`: nothing changed.", remote.repo),
    }
    Ok(())
}

/// `kito people`: the server's people, for its admins.
fn cmd_people(cli: &Cli, command: Option<&PeopleCmd>) -> Outcome {
    let (remote, mut client) = server_here(cli, false)?;
    let said: Value = match command {
        None => client.call("server.people", json!({})),
        Some(PeopleCmd::Add { name, role, email, identity }) => client.call(
            "server.people_add",
            json!({ "user": name, "role": role, "email": email, "identity": identity }),
        ),
        Some(PeopleCmd::Remove { name }) => client.call("server.people_remove", json!({ "user": name })),
        Some(PeopleCmd::Role { name, role }) => {
            client.call("server.people_role", json!({ "user": name, "role": role }))
        }
    }
    .map_err(fail)?;
    if cli.json {
        print_json(&said);
        return Ok(());
    }
    match command {
        None => {
            for person in said.as_array().into_iter().flatten() {
                let access: Vec<String> = person["access"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(repo, role)| format!("{repo} ({})", role.as_str().unwrap_or("?")))
                    .collect();
                let admin = matches!(person["role"].as_str(), Some("admin" | "owner"));
                let access = match (access.is_empty(), admin) {
                    (true, true) => "every repository".to_owned(),
                    (true, false) => "no repository given".to_owned(),
                    (false, true) => format!("every repository; given {}", access.join(", ")),
                    (false, false) => access.join(", "),
                };
                println!(
                    "{:<16} {:<7} {access}",
                    person["user"].as_str().unwrap_or("?"),
                    person["role"].as_str().unwrap_or("?")
                );
            }
        }
        Some(PeopleCmd::Add { name, .. }) => {
            println!("{name} added to {}, as {}.", remote.place(), said["role"].as_str().unwrap_or("member"));
            if let Some(token) = said["token"].as_str() {
                println!("Their token, shown this once:\n{token}");
            }
            println!("`kito grant {name} <role>` in a directory connected to a repository lets them use it.");
        }
        Some(PeopleCmd::Remove { name }) => {
            println!("{name} removed: their token and their sessions no longer work.")
        }
        Some(PeopleCmd::Role { name, role }) => {
            println!("{name} is {role} on {} from their next connection.", remote.place())
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
        let state = match (text("state"), lane["proposed_by"].as_str(), lane["review"].as_str()) {
            // Reviewed, and waiting only for somebody who may integrate.
            ("awaiting_approval", _, Some(review)) if review.starts_with("approved") => {
                format!("{review}, to integrate")
            }
            ("awaiting_approval", Some(by), Some(review)) => {
                format!("awaiting approval (asked by {by}; {review})")
            }
            ("awaiting_approval", Some(by), None) => format!("awaiting approval (asked by {by})"),
            (state, _, Some(review)) => format!("{} ({review})", state.replace('_', " ")),
            (state, ..) => state.replace('_', " "),
        };
        // What its agent runs in, as that side said it (ADR 0010).
        let with = match (lane["assistant"]["name"].as_str(), lane["assistant"]["model"].as_str()) {
            (Some(name), Some(model)) => format!(" in {name} ({model})"),
            (Some(name), None) => format!(" in {name}"),
            _ => String::new(),
        };
        println!("{}  {}{whose}{with}  {state}  {}", text("id"), text("agent"), text("intent"));
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
        LaneCmd::Open { agent, intent, scope, task, base, on, target, assistant, model } => {
            let mut params = json!({
                "agent": agent, "task": task, "base": base, "intent": intent, "scope": scope,
                "on": on, "target": target,
            });
            if assistant.is_some() {
                params["assistant"] = json!({ "name": assistant, "model": model });
            }
            let opened: Value = client.call("open_lane", params).map_err(fail)?;
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
        LaneCmd::Comment { lane, text, on, reply } => {
            let said: Value = client
                .call("comment", json!({ "lane": lane, "text": text, "on": on, "reply_to": reply }))
                .map_err(fail)?;
            if cli.json {
                print_json(&said);
                return Ok(());
            }
            print!("{}", comments_text(std::slice::from_ref(&said)));
        }
        LaneCmd::Comments { lane, all } => {
            let said: Value = client.call("comments", json!({ "lane": lane, "all": all })).map_err(fail)?;
            if cli.json {
                print_json(&said);
                return Ok(());
            }
            match said.as_array().map_or(&[][..], Vec::as_slice) {
                [] if *all => println!("Nobody commented on it."),
                [] => println!("No comment waits on it."),
                said => print!("{}", comments_text(said)),
            }
        }
        LaneCmd::ResolveComment { comment } => {
            let done: Value = client.call("resolve_comment", json!({ "comment": comment })).map_err(fail)?;
            if cli.json {
                print_json(&done);
                return Ok(());
            }
            println!("{} is resolved.", done["id"].as_str().unwrap_or(comment));
        }
        LaneCmd::Approve { .. }
        | LaneCmd::RequestChanges { .. }
        | LaneCmd::Withdraw { .. }
        | LaneCmd::Review { .. } => {
            let review: Value = match command {
                LaneCmd::Approve { lane, text } => {
                    client.call("verdict", json!({ "lane": lane, "verdict": "approve", "text": text }))
                }
                LaneCmd::RequestChanges { lane, text } => client
                    .call("verdict", json!({ "lane": lane, "verdict": "request_changes", "text": text })),
                LaneCmd::Withdraw { lane } => {
                    client.call("verdict", json!({ "lane": lane, "verdict": "withdraw" }))
                }
                LaneCmd::Review { lane } => client.call("review", json!({ "lane": lane })),
                _ => unreachable!("matched above"),
            }
            .map_err(fail)?;
            if cli.json {
                print_json(&review);
                return Ok(());
            }
            print!("{}", review_text(&review));
        }
        LaneCmd::Diff { lane, paths, stat } => {
            let diff: Value =
                client.call("lane_diff", json!({ "lane": lane, "paths": paths })).map_err(fail)?;
            if cli.json {
                print_json(&diff);
                return Ok(());
            }
            print!("{}", lane_diff_text(&diff, *stat));
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
            if let Some(said) = status["comments"].as_array().filter(|said| !said.is_empty()) {
                println!("Comments:");
                comments_text(said).lines().for_each(|line| println!("  {line}"));
            }
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
        Integration::AwaitingApproval if result.missing.is_some() => {
            println!(
                "{}: merged and checked{verified}; it waits: {}",
                result.lane,
                result.missing.as_deref().unwrap_or_default()
            );
            println!(
                "  `kito integrate {}` asks again; for a review, `kito lane review {}` says where it stands.",
                result.lane, result.lane
            );
        }
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
        // The plugins this repository enabled, and what each last failed at.
        let plugins: Value = client.call("plugins", json!({})).unwrap_or_default();
        for plugin in plugins.as_array().into_iter().flatten().filter(|plugin| plugin["enabled"] == true) {
            let text = |key: &str| plugin[key].as_str().unwrap_or_default();
            let state = match (plugin["problem"].as_str(), plugin["last_failure"].as_str()) {
                (Some(problem), _) => format!(": {problem}"),
                (None, Some(failed)) => format!(": last failed {failed}"),
                (None, None) => format!(" ({})", strings(&plugin["permissions"]).join(", ")),
            };
            println!("Plugin ............ {} {}{state}", text("name"), text("version"));
        }
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

/// The command line runs on a thread with room to spare: the main thread
/// of a Windows program has 1 MB of stack, which the commands, as many as
/// they are now, do not fit in when built without optimization.
fn main() -> ExitCode {
    const STACK: usize = 16 * 1024 * 1024;
    let ran = std::thread::Builder::new().stack_size(STACK).spawn(run).map(std::thread::JoinHandle::join);
    match ran {
        Ok(Ok(code)) => code,
        // It panicked, and said so.
        Ok(Err(_)) => ExitCode::from(101),
        Err(e) => {
            eprintln!("kito: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> ExitCode {
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
            HookCmd::BeforeEdit { file, lines, assistant, model, session } => {
                let said = hook::Said {
                    assistant: assistant.clone(),
                    model: model.clone(),
                    session: session.clone(),
                    guess: None,
                };
                hook::before_edit(file.as_deref(), lines, said)
            }
            HookCmd::AfterShell => hook::after_shell(),
            HookCmd::SessionStart { assistant } => {
                hook::session_start(hook::Said { assistant: assistant.clone(), ..hook::Said::default() })
            }
        };
    }
    // An assistant set up by its plugin: the exit code is the plugin's.
    if let Cmd::Assistant { command } = &cli.command {
        let done = cmd_assistant(&cli, command);
        if matches!(command, AssistantCmd::Add { .. } | AssistantCmd::Remove { .. }) && done.is_ok() {
            instructions_written(&cli);
        }
        return match done {
            Ok(code) => ExitCode::from(code),
            Err(e) => {
                eprintln!("kito: {e}");
                ExitCode::FAILURE
            }
        };
    }
    // A command of a plugin: its exit code is the plugin's.
    if let Cmd::External(line) = &cli.command {
        return match cmd_external(&cli, line) {
            Ok(code) => ExitCode::from(code),
            Err(e) => {
                eprintln!("kito: {e}");
                ExitCode::FAILURE
            }
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
        Cmd::Init { path, fresh, no_claude, assistants, no_assistants } => cmd_init(
            &cli,
            path,
            *fresh,
            &Chosen { no_claude: *no_claude, names: assistants.clone(), none: *no_assistants },
        ),
        Cmd::Import { path, no_claude, assistants, no_assistants } => cmd_import(
            &cli,
            path,
            &Chosen { no_claude: *no_claude, names: assistants.clone(), none: *no_assistants },
        ),
        Cmd::Claude => cmd_claude(&cli),
        Cmd::Instructions { command } => cmd_instructions(&cli, command.as_ref()),
        Cmd::Task { command } => cmd_task(&cli, command.as_ref()),
        Cmd::Search { words, kinds, since, until, who, place, outcome, limit, everywhere } => {
            cmd_search(&cli, words, kinds, since, until, who, place, outcome, *limit, *everywhere)
        }
        Cmd::Stats { since, until, everywhere } => cmd_stats(&cli, since, until, *everywhere),
        Cmd::Members => cmd_members(&cli),
        Cmd::Grant { name, role } => cmd_grant(&cli, name, Some(role)),
        Cmd::Revoke { name } => cmd_grant(&cli, name, None),
        Cmd::People { command } => cmd_people(&cli, command.as_ref()),
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
        Cmd::Connect {
            server,
            name,
            dir,
            token,
            sign_in,
            fingerprint,
            authority,
            plain,
            no_claude,
            assistants,
            no_assistants,
        } => {
            let chosen = Chosen { no_claude: *no_claude, names: assistants.clone(), none: *no_assistants };
            // Who the server is comes first: nobody signs in to another.
            address_of(server, name.as_deref(), dir.clone()).and_then(|(server, name, dir)| {
                remote_for(&server, &name, token, sign_in)
                    .and_then(|remote| trusted(remote, fingerprint, *authority, *plain))
                    .and_then(|remote| signed_in(remote, sign_in))
                    .and_then(|remote| cmd_connect(&cli, remote, &name, &dir, &chosen))
            })
        }
        Cmd::SignIn { method } => cmd_sign_in(&cli, method),
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
        Cmd::Plugin { command } => cmd_plugin(&cli, command),
        Cmd::Registry { command } => cmd_registry(&cli, command),
        Cmd::External(_) | Cmd::Assistant { .. } => unreachable!("answered above"),
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
        let tcp = remote_for("kito.example.com:7421", "shop", &Some("kito_abc".into()), &None).unwrap();
        assert_eq!(
            (tcp.server.as_deref(), tcp.token.as_deref()),
            (Some("kito.example.com:7421"), Some("kito_abc"))
        );
        assert!(tcp.command.is_empty());
        let ssh = remote_for("ssh://ada@kito.example.com/srv/kito", "shop", &None, &None).unwrap();
        assert_eq!(
            ssh.command,
            ["ssh", "ada@kito.example.com", "kito-server", "--dir", "/srv/kito", "stdio"]
        );
        assert_eq!((ssh.server, ssh.token, ssh.repo.as_str()), (None, None, "shop"));
        assert!(remote_for("ssh://ada@host", "shop", &None, &None).is_err());
        // Signing in is for a server reached as host:port, and wants no token.
        let address = |given: &str, second: Option<&str>| super::address_of(given, second, None);
        assert_eq!(address("kito.example.com/shop", None).unwrap().0, "kito.example.com:7421");
        assert_eq!(
            address("kito://kito.example.com:9000/shop/", None).unwrap(),
            ("kito.example.com:9000".into(), "shop".into(), None)
        );
        assert_eq!(address("[::1]:7421/shop", None).unwrap().0, "[::1]:7421");
        assert_eq!(address("host/shop", Some("here")).unwrap().2, Some(std::path::PathBuf::from("here")));
        assert_eq!(address("host:7421", Some("shop")).unwrap(), ("host:7421".into(), "shop".into(), None));
        assert_eq!(address("ssh://ada@host/srv/kito", Some("shop")).unwrap().1, "shop");
        assert!(address("host:7421", None).unwrap_err().contains("say the repository too"));
        assert!(address("https://kito.example.com/shop", None).unwrap_err().contains("web page"));
        assert!(address("host/a/b", None).is_err() && address("/shop", None).is_err());
        let signs = remote_for("kito.example.com:7421", "shop", &None, &Some("github".into())).unwrap();
        assert_eq!((signs.token, signs.sign_in.as_deref()), (None, Some("github")));
        assert!(remote_for("ssh://ada@host/srv/kito", "shop", &None, &Some("github".into())).is_err());
    }

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00");
        assert_eq!(utc(1_794_659_467_000), "2026-11-14 12:31:07");
        assert_eq!(utc(4_102_444_799_999), "2099-12-31 23:59:59");
    }
}
