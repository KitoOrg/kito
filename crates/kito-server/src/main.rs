//! `kito-server`: one server for several repositories.
//!
//! The repositories live under the server's directory, each an ordinary
//! Kito repository with its own daemon. The server is the door in front of
//! them: it knows who a client is, which repositories that person may use,
//! and passes the requests of a session on to the daemon of the repository
//! the session is about, with the person's name on them.
//!
//! ```text
//! kito-server init /srv/kito
//! kito-server --dir /srv/kito repo add shop --from /path/to/git/repository
//! kito-server --dir /srv/kito user add ada --email ada@example.com
//! kito-server --dir /srv/kito serve --listen 127.0.0.1:7421
//! ```
//!
//! Clients reach it over TCP with a token (`serve`), or through a command
//! whose standard input and output carry the protocol (`stdio`), which is
//! how it is used over SSH: there the person is who SSH says they are.
//!
//! The protocol is one JSON message per line. The first line of a client
//! is `{"hello": {"version": 1, "token": "...", "repo": "shop"}}`; the
//! server answers `{"ok": {...}}` or `{"error": "..."}`. After that come
//! the requests of the daemon's own protocol (JSON-RPC 2.0).

mod admin;
mod audit;
mod backup;
mod http;
mod session;
mod state;
mod tls;

use clap::{Parser, Subcommand};
use kitod::client::Client;
use serde_json::{json, Value};
use state::{Role, Server, ServerRole, User};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "kito-server", version, about = "One Kito server for several repositories")]
struct Cli {
    /// The server's directory (default: $KITO_SERVER_DIR, then the current directory).
    #[arg(long, global = true, value_name = "DIR")]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum PluginCmd {
    /// Install a plugin for the user this server runs as: by its name,
    /// from the registry, or from a directory that holds its
    /// `kito-plugin.toml` and its program
    Add {
        /// The plugin, as the registry lists it: `web`, or `web@0.4.0`
        #[arg(required_unless_present = "path", conflicts_with = "path")]
        name: Option<String>,
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
    },
    /// Let a plugin run on this server, with what it asks for
    Enable {
        name: String,
        /// Give it these permissions only: sign_in, pages, server_events,
        /// server_admin, read, write, integrate, admin
        #[arg(long, value_delimiter = ',', value_name = "PERMISSIONS")]
        only: Option<Vec<String>>,
        /// It runs in the directories connected to this server too, for
        /// whoever has it installed, and may ask its counterpart here
        #[arg(long)]
        for_clients: bool,
    },
    /// Stop a plugin from running on this server: it takes out what it
    /// set up here
    Disable {
        name: String,
        /// Remove what the plugin kept of its own on this server as well
        #[arg(long)]
        purge: bool,
        /// Do not run the plugin one last time
        #[arg(long)]
        no_cleanup: bool,
    },
    /// Remove a plugin that is not enabled from the server's machine
    Remove {
        name: String,
        /// Do not run the plugin one last time
        #[arg(long)]
        no_cleanup: bool,
    },
    /// The plugins this server enabled, and what each was given
    List,
}

#[derive(Subcommand)]
enum Cmd {
    /// Plugins of the server: signing in, pages, events, commands
    Plugin {
        #[command(subcommand)]
        command: PluginCmd,
    },
    /// A command of a plugin this server enabled
    #[command(external_subcommand)]
    External(Vec<OsString>),
    /// Create the directory of a server
    Init {
        /// Where (default: --dir, then the current directory)
        path: Option<PathBuf>,
    },
    /// The repositories the server hosts
    Repo {
        #[command(subcommand)]
        command: RepoCmd,
    },
    /// The people who may use the server
    User {
        #[command(subcommand)]
        command: UserCmd,
    },
    /// Let a person use a repository, as read, write, integrate or
    /// maintain (ADR 0014); again to change it
    Grant {
        name: String,
        repo: String,
        /// read, write, integrate, maintain
        role: String,
    },
    /// Take a repository away from a person
    Revoke { name: String, repo: String },
    /// What changed the server and who did it (ADR 0017); the latest first
    Audit {
        /// Each request, not what changed the server (kept 90 days)
        #[arg(long)]
        requests: bool,
        #[arg(long)]
        who: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        /// From when: 7d, 12h
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value = "50")]
        limit: usize,
        /// Check that no line was taken out or changed
        #[arg(long)]
        verify: bool,
    },
    /// Backups of the whole server, encrypted: run one now (the default),
    /// list, key, schedule, s3
    Backup {
        #[command(subcommand)]
        command: Option<BackupCmd>,
    },
    /// Put a backup back, into an empty directory: then serve from there
    Restore {
        /// The archive: a file, or `s3:<name>` as `backup list` names it
        archive: String,
        #[arg(long, value_name = "DIR")]
        to: PathBuf,
        /// The backup key, as `backup key` printed it (default: this server's)
        #[arg(long, value_name = "FILE")]
        key: Option<PathBuf>,
    },
    /// Print the fingerprint of the server's certificate: what clients
    /// connect with (`kito connect ... --fingerprint`)
    Fingerprint,
    /// Serve the repositories to clients that connect over TCP, with a
    /// token; connections are encrypted with the server's certificate
    Serve {
        /// Do not encrypt: for clients on this machine only
        #[arg(long)]
        plain: bool,
        /// Address to listen on (default: the one in server.toml)
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,
        /// Answer HTTP on this address too: who is signed in, a
        /// repository asked as that person, and the pages of plugins
        /// (default: `http` in server.toml, if it is there)
        #[arg(long, value_name = "ADDR")]
        http: Option<String>,
    },
    /// Serve one client over standard input and output: for SSH
    Stdio {
        /// The person the client is: SSH's forced command says it
        /// (default: the name of the system user)
        #[arg(long = "as", value_name = "USER")]
        user: Option<String>,
    },
}

#[derive(Subcommand)]
enum RepoCmd {
    /// Add a repository: with a README.md to start from, or with the
    /// history of a git repository
    Add {
        name: String,
        /// A git repository to clone and import: a path or a URL
        #[arg(long, value_name = "GIT")]
        from: Option<String>,
        /// Leave the main lane open to direct edits on the server (a
        /// repository added with a history is protected otherwise)
        #[arg(long)]
        unprotected: bool,
    },
    /// List the repositories, with the rules of each
    List,
    /// Take a repository out of use: hidden, and deleted after 30 days
    Archive { name: String },
    /// Bring an archived repository back
    Unarchive { name: String },
    /// Delete a repository for good (the backups still have it)
    Delete {
        name: String,
        /// Its name again, to say it is meant
        #[arg(long, value_name = "NAME")]
        confirm: String,
        /// The archived one of that name, when one in use has it too
        #[arg(long, conflicts_with = "live")]
        archived: bool,
        /// The one in use, when an archived one has its name too
        #[arg(long)]
        live: bool,
    },
    /// Rename a repository: directories connected with the old name still reach it
    Rename { from: String, to: String },
    /// Set the rules of a repository: protection, checks, git push
    Set {
        name: String,
        /// The main lane changes only when a lane is integrated
        #[arg(long, conflicts_with = "unprotect")]
        protect: bool,
        /// The main lane may be edited directly on the server again
        #[arg(long)]
        unprotect: bool,
        /// A command that must succeed on the merged result before a lane
        /// is integrated; once per command, in the order they run. Given at
        /// all, they replace the ones set before
        #[arg(long = "check", value_name = "COMMAND")]
        checks: Vec<String>,
        /// No command has to succeed
        #[arg(long, conflicts_with = "checks")]
        no_checks: bool,
        /// Approvals a lane needs before it is integrated, from people
        /// other than its owner (0: none)
        #[arg(long, value_name = "N")]
        approvals: Option<u32>,
        /// Push every integration to this git remote: a name the
        /// repository's git knows, or a URL
        #[arg(long, value_name = "REMOTE", conflicts_with = "no_push")]
        push: Option<String>,
        /// Stop pushing
        #[arg(long)]
        no_push: bool,
        /// The kitchen whose instructions its agents get, before the
        /// repository's own: another repository of this server
        #[arg(long, value_name = "REPOSITORY", conflicts_with = "no_kitchen")]
        kitchen: Option<String>,
        /// The standard kitchen: Kito's rules, and the repository's own
        /// instructions
        #[arg(long)]
        no_kitchen: bool,
    },
}

#[derive(Subcommand)]
enum BackupCmd {
    /// Make one now
    Run,
    /// The archives, here and in S3
    List,
    /// Print the backup key: keep it somewhere else, a restore needs it
    Key,
    /// How often, and how many are kept
    Schedule {
        /// Hours between two (0: only when asked)
        #[arg(long)]
        every: Option<u64>,
        #[arg(long)]
        keep: Option<usize>,
        /// The folder for the archives; `none`: only in S3
        #[arg(long)]
        dir: Option<String>,
    },
    /// Send them to storage that speaks S3 too (or --off)
    S3 {
        #[arg(long, required_unless_present = "off")]
        endpoint: Option<String>,
        #[arg(long, required_unless_present = "off")]
        bucket: Option<String>,
        #[arg(long, default_value = "auto")]
        region: String,
        #[arg(long, default_value = "")]
        prefix: String,
        /// A file with two lines: the access key, then the secret key
        #[arg(long, value_name = "FILE")]
        keys: Option<PathBuf>,
        #[arg(long)]
        off: bool,
    },
}

#[derive(Subcommand)]
enum UserCmd {
    /// Add a person and print their token, once
    Add {
        name: String,
        #[arg(long)]
        email: Option<String>,
        /// A repository they may use, and as what: shop (write),
        /// shop:read, shop:integrate, shop:maintain. Again for more
        #[arg(long = "repo", value_name = "REPO[:ROLE]")]
        repos: Vec<String>,
        /// Their role on the server: member (the repositories given),
        /// admin (every repository, and the people), owner
        #[arg(long, default_value = "member")]
        role: String,
        /// Who they are for a plugin that signs people in, as
        /// <method>:<login> (github:bob): they sign in, and have no token
        #[arg(long, value_name = "METHOD:LOGIN")]
        identity: Option<String>,
    },
    /// Change a person's role on the server: member, admin, owner
    Role { name: String, role: String },
    /// Attach an identity to a person: whoever signs in as it is them,
    /// and they keep their token
    Link {
        name: String,
        /// Who they are for a plugin that signs people in, as
        /// <method>:<login> (github:bob)
        #[arg(value_name = "METHOD:LOGIN")]
        identity: String,
    },
    /// Detach a person's identity of a way to sign in: their sessions end
    Unlink {
        name: String,
        /// The way to sign in: github
        method: String,
    },
    /// Give a person a new token, printed once: the one they had stops
    /// working at once
    Token {
        name: String,
        /// Take their token away instead: they sign in from then on
        #[arg(long)]
        remove: bool,
        /// For a token that leaked: the new token is all they sign in
        /// with. Every identity of theirs is unlinked, whoever linked it,
        /// and every session ends, in one change
        #[arg(long, conflicts_with = "remove")]
        alone: bool,
    },
    /// List the people, how each signs in, their role on the server, and
    /// what they may do where
    List,
    /// Remove a person: their token stops working
    Remove { name: String },
}

/// What tells clients the server is the one meant, on a `kito connect`
/// line: nothing for a certificate from an authority, else its fingerprint.
fn known_by(dir: &std::path::Path) -> Result<String, String> {
    match tls::from_authority(dir) {
        true => Ok(String::new()),
        false => Ok(format!(" --fingerprint {}", tls::fingerprint(dir)?)),
    }
}

/// `7d`, `12h`, `2w` before now, in milliseconds since 1970.
fn ago(given: &str) -> Result<u64, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    let (number, unit) = given.trim().split_at(given.trim().len().saturating_sub(1));
    let size = match unit {
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        _ => return Err(format!("`{given}` is no time: say 12h, 7d or 2w")),
    };
    let n: u64 = number.parse().map_err(|_| format!("`{given}` is no time: say 12h, 7d or 2w"))?;
    Ok(now.saturating_sub(n * size))
}

/// How a person signs in now, in a line: never a secret.
fn signs_in(server: &Server, name: &str) -> String {
    let ways: Vec<String> = server
        .config
        .users
        .get(name)
        .map(User::signs_in_with)
        .unwrap_or_default()
        .into_iter()
        .map(|way| if way == "token" { "with a token".to_owned() } else { format!("as {way}") })
        .collect();
    format!("{name} signs in {}.", ways.join(" or "))
}

/// What a person is told after their token was replaced, when they
/// keep an identity: whoever held the token may have attached an account
/// of their own to them, and a new token does not take it away.
fn if_it_leaked(server: &Server, name: &str) {
    let logins = server.config.users.get(name).map(|user| user.logins.clone()).unwrap_or_default();
    if !logins.is_empty() {
        println!(
            "If it was replaced because somebody else had it: look at each identity ({}) and unlink what is not \
             theirs (`kito-server user unlink {name} <method>`), or give them a token alone (`kito-server user \
             token {name} --alone`).",
            logins.join(", ")
        );
    }
}

/// Who changed a setting with `kito-server` itself, on the server's machine.
const BY_HAND: &str = "the server's machine";

fn dir_of(cli: &Cli) -> PathBuf {
    cli.dir
        .clone()
        .or_else(|| std::env::var_os("KITO_SERVER_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// What a plugin adds, in a few words, for the lists.
fn adds(manifest: &kitod::plugins::Manifest) -> Vec<String> {
    let mut adds = Vec::new();
    if let Some(sign_in) = &manifest.sign_in {
        adds.push(format!("signing in with {}", sign_in.method));
    }
    if let Some(pages) = &manifest.pages {
        adds.push(format!("pages under {}", pages.mount));
    }
    let kinds: Vec<&str> =
        manifest.server_events.iter().flat_map(|event| event.kinds.iter().map(String::as_str)).collect();
    if !kinds.is_empty() {
        adds.push(format!("run for {}", kinds.join(", ")));
    }
    adds.extend(manifest.server_commands.iter().map(|command| format!("kito-server {}", command.name)));
    adds
}

/// Says how a plugin's last run went: what it printed, or that it failed,
/// which stops nothing.
fn said_cleaning(name: &str, place: &str, ran: Option<Result<String, String>>) {
    match ran {
        None => {}
        Some(Ok(printed)) => {
            for line in printed.lines() {
                println!("  {line}");
            }
        }
        Some(Err(why)) => eprintln!(
            "kito-server: the plugin `{name}` did not take out what it wrote {place} ({why}): it goes all the same"
        ),
    }
}

fn run_plugin(cli: &Cli, command: &PluginCmd) -> Result<(), String> {
    use kitod::plugins;
    match command {
        PluginCmd::Add { name, path } => {
            let (added, source) = match (name, path) {
                (_, Some(path)) => (plugins::install_from(path)?, None),
                (Some(name), None) => {
                    let (name, version) = match name.split_once('@') {
                        Some((name, version)) => (name, Some(version)),
                        None => (name.as_str(), None),
                    };
                    if let Some(why) = kitod::registry::not_listed_for(name, "server") {
                        return Err(why);
                    }
                    let (added, source) = kitod::registry::install(name, version)?;
                    (added, Some(source))
                }
                (None, None) => return Err("say which plugin: its name, or --path <dir>".to_owned()),
            };
            if let Some(source) = &source {
                println!("from {} (SHA-256 {})", source.url, source.sha256);
            }
            println!(
                "{} {} is installed in {}",
                added.manifest.name,
                added.manifest.version,
                added.dir.display()
            );
            println!("  `kito-server plugin enable {}` lets it run on this server.", added.manifest.name);
        }
        PluginCmd::Enable { name, only, for_clients } => {
            let only = match only {
                None => None,
                Some(words) => Some(
                    words
                        .iter()
                        .map(|word| {
                            serde_json::from_value(json!(word))
                                .map_err(|_| format!("`{word}` is not a permission"))
                        })
                        .collect::<Result<Vec<plugins::Permission>, String>>()?,
                ),
            };
            let mut server = Server::open(&dir_of(cli))?;
            let found = server.enable_plugin(name, only, *for_clients)?;
            let given: Vec<&str> = server.config.plugins[name].permissions.iter().map(|p| p.word()).collect();
            let given = if given.is_empty() { "nothing".to_owned() } else { given.join(", ") };
            println!("{name} is enabled on this server. It may: {given}.");
            if *for_clients {
                println!(
                    "  it runs in the directories connected to this server too, for whoever has it installed"
                );
            }
            for what in adds(&found.manifest) {
                println!("  {what}");
            }
        }
        PluginCmd::Disable { name, purge, no_cleanup } => {
            let mut server = Server::open(&dir_of(cli))?;
            let enabled = server.config.plugins.get(name).is_some_and(|plugin| plugin.enabled);
            // One last time, for what it set up on this server.
            if let (true, false, Some(found)) = (enabled, *no_cleanup, plugins::installed(name)) {
                let envs = [(state::ENV_SERVER_DIR, server.dir.as_os_str().to_owned())];
                said_cleaning(name, "on this server", plugins::cleanup(&found, "server", &server.dir, &envs));
            }
            match (server.disable_plugin(name, *purge)?, purge) {
                (true, true) => {
                    println!("{name} is disabled on this server, and what it kept here is removed.")
                }
                (true, false) => println!("{name} is disabled on this server."),
                (false, _) => println!("{name} was not enabled on this server."),
            }
        }
        PluginCmd::Remove { name, no_cleanup } => {
            let mut server = Server::open(&dir_of(cli))?;
            // Disabled first: what runs is not pulled from under a server.
            server.forget_plugin(name)?;
            let found =
                plugins::installed(name).ok_or_else(|| format!("the plugin `{name}` is not installed"))?;
            if !*no_cleanup {
                said_cleaning(
                    name,
                    "for the server's user",
                    plugins::cleanup(&found, "person", &found.dir, &[]),
                );
            }
            plugins::uninstall(name)?;
            println!("{name} is removed from this machine.");
        }
        PluginCmd::List => {
            let server = Server::open(&dir_of(cli))?;
            if server.config.plugins.is_empty() {
                println!("No plugins. `kito-server plugin add --path <dir>`, then `plugin enable <name>`.");
            }
            for (name, config) in &server.config.plugins {
                let found = plugins::installed(name);
                let version =
                    found.as_ref().map(|found| format!(" {}", found.manifest.version)).unwrap_or_default();
                let given: Vec<&str> = config.permissions.iter().map(|p| p.word()).collect();
                let state = match (config.enabled, config.for_clients) {
                    (true, true) => format!("enabled ({}), for clients too", given.join(", ")),
                    (true, false) => format!("enabled ({})", given.join(", ")),
                    (false, _) => "disabled".to_owned(),
                };
                println!("{name}{version}  [{state}]");
                match &found {
                    None => println!("  it is not installed for the user this server runs as"),
                    Some(found) => {
                        if let Some(why) = found.manifest.unfit() {
                            println!("  {why}");
                        }
                        for what in adds(&found.manifest) {
                            println!("  {what}");
                        }
                    }
                }
                if let Some(failed) = server.last_failure(name) {
                    println!("  it last failed {failed}");
                }
            }
        }
    }
    Ok(())
}

/// Runs a command a plugin adds to `kito-server`, with the server's
/// directory in its environment. Returns its exit code.
fn run_external(cli: &Cli, line: &[OsString]) -> Result<u8, String> {
    use kitod::plugins::{self, Permission};
    let asked = line.first().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let server = Server::open(&dir_of(cli)).map_err(|e| {
        format!("`{asked}` is not a command of kito-server (`kito-server --help` lists them); {e}")
    })?;
    let declares =
        |manifest: &plugins::Manifest| manifest.server_commands.iter().any(|command| command.name == asked);
    let running = server.running();
    let Some((found, config)) = running.iter().find(|(found, _)| declares(&found.manifest)) else {
        let has = plugins::all_installed().into_iter().find(|plugin| declares(&plugin.manifest));
        return Err(match has {
            Some(plugin) => format!(
                "`kito-server {asked}` is a command of the plugin `{}`, which this server did not enable: \
                 `kito-server plugin enable {}`",
                plugin.manifest.name, plugin.manifest.name
            ),
            None => format!("`{asked}` is not a command of kito-server (`kito-server --help` lists them)"),
        });
    };
    let name = &found.manifest.name;
    if !config.permissions.contains(&Permission::ServerAdmin) {
        return Err(format!(
            "the plugin `{name}` may not add commands to this server: it needs the permission `server_admin` \
             (`kito-server plugin enable {name}` gives what it asks for)"
        ));
    }
    let program = plugins::program(&found.dir, name);
    let mut command = std::process::Command::new(&program);
    command
        .args(line)
        .current_dir(&server.dir)
        .env(state::ENV_SERVER_DIR, &server.dir)
        .env(plugins::ENV_API, kitod::rpc::API.to_string());
    // On Unix `kito-server` becomes the plugin: one process to stop.
    #[cfg(unix)]
    {
        let failed = std::os::unix::process::CommandExt::exec(&mut command);
        Err(format!("{}: {failed}", program.display()))
    }
    #[cfg(not(unix))]
    {
        let status = command.status().map_err(|e| format!("{}: {e}", program.display()))?;
        Ok(status.code().map_or(128, |code| code.clamp(0, 255) as u8))
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    match &cli.command {
        Cmd::Plugin { command } => run_plugin(cli, command)?,
        Cmd::External(_) => unreachable!("answered before"),
        Cmd::Init { path } => {
            let dir = path.clone().unwrap_or_else(|| dir_of(cli));
            let server = Server::init(&dir)?;
            println!("Kito server created in {}", server.dir.display());
            println!("Its certificate's fingerprint: {}", tls::fingerprint(&server.dir)?);
            println!("Add a repository with `kito-server --dir {} repo add <name>`,", server.dir.display());
            println!("a person with `user add <name>`, then run `serve`.");
        }
        Cmd::Repo { command: RepoCmd::Add { name, from, unprotected } } => {
            let server = Server::open(&dir_of(cli))?;
            let added = vec![("repo_added", json!({ "repo": name, "what": name }))];
            let root = server.add_repo_told(name, from.as_deref(), None, !unprotected, added)?;
            println!("Repository `{name}` created in {}", root.display());
            match (from, unprotected) {
                (Some(_), false) => {
                    println!("Its main lane is protected: it changes only when a lane is integrated.")
                }
                (Some(_), true) => {}
                (None, _) => {
                    println!("It starts with a README.md, for lanes to start from.");
                    println!(
                        "Its main lane is open to direct edits: `kito-server repo set {name} --protect` protects it."
                    );
                }
            }
        }
        Cmd::Repo { command: RepoCmd::Archive { name } } => {
            let server = Server::open(&dir_of(cli))?;
            server.archive_repo(name, None)?;
            println!("`{name}` is archived: nobody reaches it, and it is deleted in {} days (`repo unarchive {name}` brings it back).", state::KEEP_ARCHIVED_DAYS);
        }
        Cmd::Repo { command: RepoCmd::Unarchive { name } } => {
            let server = Server::open(&dir_of(cli))?;
            server.unarchive_repo(name, None)?;
            println!("`{name}` is back, as it was.");
        }
        Cmd::Repo { command: RepoCmd::Delete { name, confirm, archived, live } } => {
            if confirm != name {
                return Err(format!("to delete `{name}`, give its name again: --confirm {name}"));
            }
            let mut server = Server::open(&dir_of(cli))?;
            let asked = match (archived, live) {
                (true, _) => Some(state::Which::Archived),
                (_, true) => Some(state::Which::Live),
                _ => None,
            };
            let said = match server.delete_repo(name, asked, None)? {
                state::Which::Live => format!("`{name}` is deleted"),
                state::Which::Archived => format!("The archived `{name}` is deleted"),
            };
            println!("{said}. The backups made before still have it.");
        }
        Cmd::Repo { command: RepoCmd::Rename { from, to } } => {
            let mut server = Server::open(&dir_of(cli))?;
            server.rename_repo(from, to, None)?;
            println!("`{from}` is `{to}` now; what is connected to `{from}` still reaches it.");
        }
        Cmd::Repo { command: RepoCmd::List } => {
            let server = Server::open(&dir_of(cli))?;
            let repos = server.repos();
            if repos.is_empty() {
                println!("No repositories. Add one with `kito-server repo add <name>`.");
            }
            for name in repos {
                // The rules are the repository's own: read from its
                // configuration, without waking its daemon.
                let rules = server
                    .repo(&name)
                    .and_then(|root| kitod::config::Config::load(&root.join(kito_store::STORE_DIR)).ok())
                    .map(|config| {
                        rules_of(
                            config.main.protected,
                            &config.verify.commands,
                            config.review.required,
                            config.git.push.as_deref(),
                        )
                    })
                    .unwrap_or_default();
                println!("{name}{rules}");
            }
        }
        Cmd::Repo {
            command:
                RepoCmd::Set {
                    name,
                    protect,
                    unprotect,
                    checks,
                    no_checks,
                    approvals,
                    push,
                    no_push,
                    kitchen,
                    no_kitchen,
                },
        } => {
            let server = Server::open(&dir_of(cli))?;
            let root =
                server.repo(name).ok_or_else(|| format!("there is no repository `{name}` on this server"))?;
            // Through the repository's daemon, which holds the rules it applies.
            let mut daemon = Client::connect_or_start(&root).map_err(|e| e.to_string())?;
            if *protect || *unprotect {
                let _: Value =
                    daemon.call("protect", json!({ "on": *protect })).map_err(|e| e.to_string())?;
                server.note_setting(name, BY_HAND, "protected", &json!(*protect));
            }
            let mut rules = json!({});
            if !checks.is_empty() || *no_checks {
                rules["checks"] = json!(checks);
            }
            if push.is_some() || *no_push {
                rules["push"] = json!(push);
            }
            if let Some(approvals) = approvals {
                rules["approvals"] = json!(approvals);
            }
            // A kitchen is a repository of the server, next to this one.
            if let Some(kitchen) = kitchen {
                if kitchen == name {
                    return Err(format!("`{name}` cannot be its own kitchen"));
                }
                server.repo(kitchen).ok_or_else(|| {
                    format!("there is no repository `{kitchen}` on this server for a kitchen")
                })?;
                rules["kitchen"] = json!(format!("../{kitchen}"));
            } else if *no_kitchen {
                rules["kitchen"] = Value::Null;
            }
            let now: Value = daemon.call("configure", rules.clone()).map_err(|e| e.to_string())?;
            for (setting, value) in rules.as_object().into_iter().flatten() {
                server.note_setting(name, BY_HAND, setting, value);
            }
            let checks: Vec<String> = serde_json::from_value(now["checks"].clone()).unwrap_or_default();
            let rules = rules_of(
                now["protected"].as_bool().unwrap_or(false),
                &checks,
                now["approvals"].as_u64().unwrap_or(0) as u32,
                now["push"].as_str(),
            );
            println!("{name}{rules}");
            if kitchen.is_some() || *no_kitchen {
                match now["kitchen"].as_str() {
                    Some(path) => println!(
                        "  its agents get the instructions of the kitchen {}",
                        path.trim_start_matches("../")
                    ),
                    None => println!(
                        "  its agents get the standard kitchen: Kito's rules, and its own instructions"
                    ),
                }
            }
            server.tell("repo_changed", json!({ "repo": name, "what": format!("{name}{rules}") }));
        }
        Cmd::User { command: UserCmd::Add { name, email, repos, role, identity } } => {
            let mut server = Server::open(&dir_of(cli))?;
            // Who that is, asked of the plugin now: what is kept does not
            // change when they rename themselves there.
            let identity = identity.as_deref().map(|identity| server.look_up(identity)).transpose()?;
            let role = ServerRole::of_word(role)?;
            let access = repos
                .iter()
                .map(|given| match given.split_once(':') {
                    Some((repo, role)) => Role::of_word(role).map(|role| (repo.to_owned(), role)),
                    None => Ok((given.clone(), Role::Write)),
                })
                .collect::<Result<Vec<_>, String>>()?;
            // What says the server they reach is this one: found before
            // they are added, for nobody to be added and not told how.
            let known = known_by(&server.dir)?;
            let token = server.change_told(|change| {
                let token = change.add_user(name, email.clone(), &access, role, identity.as_ref())?;
                let what = format!("{name} ({})", role.word());
                Ok((token, vec![("user_added", json!({ "user": name, "role": role.word(), "what": what }))]))
            })?;
            let first = access.first().map(|(repo, _)| repo.as_str()).unwrap_or("<repository>");
            {
                if access.is_empty() && role == ServerRole::Member {
                    println!(
                        "{name} may use no repository yet: `kito-server grant {name} <repository> <role>`."
                    );
                }
                if let Some(token) = &token {
                    println!("{name} added. Their token, shown this once:");
                    println!("{token}");
                    // What they connect with: the token, and what says the
                    // server they reach is this one.
                    println!("They connect with:");
                    println!("  kito connect <host>:<port>/{first} --token {token}{known}");
                } else if let Some(identity) = &identity {
                    let method = &identity.provider;
                    println!(
                        "{name} added: whoever signs in as {method}:{} is {name}. No token.",
                        identity.login
                    );
                    println!("They connect with:");
                    println!("  kito connect <host>:<port>/{first} --sign-in {method}{known}");
                }
            }
        }
        Cmd::User { command: UserCmd::Role { name, role } } => {
            let mut server = Server::open(&dir_of(cli))?;
            let role = ServerRole::of_word(role)?;
            server.change_told(|change| {
                change.set_server_role(name, role)?;
                let what = format!("{name}: {}", role.word());
                Ok((
                    (),
                    vec![("role_changed", json!({ "user": name, "server_role": role.word(), "what": what }))],
                ))
            })?;
            match role {
                ServerRole::Member => {
                    println!("{name} is a member: the repositories given them, nothing else.")
                }
                ServerRole::Admin => println!(
                    "{name} is an admin: every repository as maintain, its people, and the server's plugins."
                ),
                ServerRole::Owner => {
                    println!("{name} is an owner: as an admin, and names admins and owners.")
                }
            }
        }
        Cmd::User { command: UserCmd::Link { name, identity } } => {
            // Who that is, asked of the plugin now: what is kept does not
            // change when they rename themselves there.
            let identity = Server::open(&dir_of(cli))?.look_up(identity)?;
            // The server as it is once the plugin has answered.
            let mut server = Server::open(&dir_of(cli))?;
            let written = identity.written();
            let linked = server.change_told(|change| {
                let linked = change.link(name, &identity)?;
                let what = format!("{name}: {written}");
                let event = json!({ "user": name, "identity": identity.key(), "what": what });
                Ok((linked, if linked { vec![("identity_linked", event)] } else { Vec::new() }))
            })?;
            match linked {
                true => {
                    println!("Whoever signs in as {written} is {name} from now on.");
                    println!("{}", signs_in(&server, name));
                }
                false => {
                    println!("{written} is {name} already: nothing changed.");
                    println!("{}", signs_in(&server, name));
                }
            }
        }
        Cmd::User { command: UserCmd::Unlink { name, method } } => {
            let mut server = Server::open(&dir_of(cli))?;
            // Every session of theirs ends with it: one of them may have
            // been opened as who they no longer are.
            let login = server.change_told(|change| {
                let (login, ended) = change.unlink(name, method, None)?;
                let event = json!({ "user": name, "what": format!("{name}: {login}") });
                Ok((login, Server::with_sessions_ended(("identity_unlinked", event), name, ended)))
            })?;
            println!("{name} no longer signs in as {login}: their sessions ended.");
            println!("{}", signs_in(&server, name));
        }
        Cmd::User { command: UserCmd::Token { name, alone: true, .. } } => {
            let mut server = Server::open(&dir_of(cli))?;
            let known = known_by(&server.dir)?;
            let (token, unlinked) = server.change_told(|change| {
                let (token, unlinked, replaced, ended) = change.token_alone(name, None)?;
                let events = Server::alone_events(name, None, (&unlinked, replaced), ended);
                Ok(((token, unlinked), events))
            })?;
            let given = server.config.users.get(name).and_then(|user| user.access.keys().next());
            let first = given.map(String::as_str).unwrap_or("<repository>");
            {
                println!("{name}'s new token, shown this once, and all they sign in with now:");
                println!("{token}");
                println!("They connect with:");
                println!("  kito connect <host>:<port>/{first} --token {token}{known}");
                match unlinked.is_empty() {
                    true => println!("They had no identity to unlink."),
                    false => println!("Unlinked: {}.", unlinked.join(", ")),
                }
                println!(
                    "Every session of theirs ended, and what was connected with the token they had no longer \
                     works."
                );
                println!("{}", signs_in(&server, name));
            }
        }
        Cmd::User { command: UserCmd::Token { name, remove: false, .. } } => {
            let mut server = Server::open(&dir_of(cli))?;
            // Found before the token is changed: nothing fails between
            // the change and the one time the token is said.
            let known = known_by(&server.dir)?;
            let (token, replaced) = server.change_told(|change| {
                let (token, replaced, ended) = change.change_token(name, None)?;
                let what = match replaced {
                    true => name.clone(),
                    false => format!("{name}, who had none"),
                };
                let event = json!({ "user": name, "replaced": replaced, "what": what });
                Ok(((token, replaced), Server::with_sessions_ended(("token_changed", event), name, ended)))
            })?;
            // What they connect with, as when they were added.
            let given = server.config.users.get(name).and_then(|user| user.access.keys().next());
            let first = given.map(String::as_str).unwrap_or("<repository>");
            {
                match replaced {
                    true => println!("{name}'s new token, shown this once:"),
                    false => println!("{name} has a token now, shown this once:"),
                }
                println!("{token}");
                println!("They connect with:");
                println!("  kito connect <host>:<port>/{first} --token {token}{known}");
                if replaced {
                    println!(
                        "The one they had stops working now: their sessions ended, and what was connected with \
                         it is connected again with this one."
                    );
                }
                println!("{}", signs_in(&server, name));
                if replaced {
                    if_it_leaked(&server, name);
                }
            }
        }
        Cmd::User { command: UserCmd::Token { name, remove: true, .. } } => {
            let mut server = Server::open(&dir_of(cli))?;
            server.change_told(|change| {
                let ended = change.remove_token(name, None)?;
                let event = json!({ "user": name, "what": name });
                Ok(((), Server::with_sessions_ended(("token_removed", event), name, ended)))
            })?;
            println!(
                "{name} has no token any more: their sessions ended, and what was connected with it no longer \
                 works."
            );
            println!("{}", signs_in(&server, name));
        }
        Cmd::Grant { name, repo, role } => {
            let mut server = Server::open(&dir_of(cli))?;
            let role = Role::of_word(role)?;
            server.change_told(|change| {
                let changed = change.grant(name, repo, Some(role))?;
                let what = format!("{name} may {} in {repo}", role.word());
                let event = json!({ "user": name, "repo": repo, "role": role.word(), "what": what });
                Ok(((), if changed { vec![("role_changed", event)] } else { Vec::new() }))
            })?;
            println!("{name} may {} in `{repo}`.", role.word());
        }
        Cmd::Revoke { name, repo } => {
            let mut server = Server::open(&dir_of(cli))?;
            let revoked = server.change_told(|change| {
                let changed = change.grant(name, repo, None)?;
                let what = format!("{name} may no longer use {repo}");
                let event = json!({ "user": name, "repo": repo, "role": null, "what": what });
                Ok((changed, if changed { vec![("role_changed", event)] } else { Vec::new() }))
            })?;
            match revoked {
                true => println!("{name} may no longer use `{repo}`."),
                false => println!("{name} was not given `{repo}`: nothing changed."),
            }
            if server.config.users.get(name.as_str()).is_some_and(|user| user.is_admin()) {
                println!("As an admin of the server they still may use it.");
            }
        }
        Cmd::User { command: UserCmd::List } => {
            let server = Server::open(&dir_of(cli))?;
            if server.config.users.is_empty() {
                println!("Nobody yet. Add a person with `kito-server user add <name>`.");
            }
            for (name, user) in &server.config.users {
                let email = user.email.as_deref().map(|email| format!(" <{email}>")).unwrap_or_default();
                // What they may do where.
                let places: Vec<String> =
                    user.access.iter().map(|(repo, role)| format!("{repo} ({})", role.word())).collect();
                let places = match (places.is_empty(), user.is_admin()) {
                    (_, true) => vec![format!(
                        "every repository{}",
                        if places.is_empty() {
                            String::new()
                        } else {
                            format!("; given {}", places.join(", "))
                        }
                    )],
                    (true, false) => vec!["no repository".to_owned()],
                    (false, false) => places,
                };
                // How they sign in: with a token, as who a plugin says they
                // are (by the login it had when it was attached), or both.
                let ways = user.signs_in_with();
                let signs = match ways.is_empty() {
                    true => String::new(),
                    false => format!(" [{}]", ways.join(", ")),
                };
                println!("{name}{email}{signs} ({}): {}", user.server_role.word(), places.join(", "));
            }
        }
        Cmd::User { command: UserCmd::Remove { name } } => {
            let mut server = Server::open(&dir_of(cli))?;
            server.change_told(|change| {
                change.remove_user(name)?;
                Ok(((), vec![("user_removed", json!({ "user": name, "what": name }))]))
            })?;
            println!("{name} removed: their token and their sessions no longer work.");
        }
        Cmd::Fingerprint => {
            let server = Server::open(&dir_of(cli))?;
            println!("{}", tls::fingerprint(&server.dir)?);
        }
        Cmd::Audit { requests, who, kind, repo, since, limit, verify } => {
            let server = Server::open(&dir_of(cli))?;
            if *verify {
                let lines = audit::verify(&server.dir)?;
                println!("The audit log is whole: {lines} lines, each chained to the one before.");
                return Ok(());
            }
            let filter = audit::Filter {
                requests: *requests,
                who: who.clone(),
                kind: kind.clone(),
                repo: repo.clone(),
                since_ms: since.as_deref().map(ago).transpose()?,
                limit: *limit,
            };
            let found = audit::read(&server.dir, &filter);
            if found.is_empty() {
                println!("Nothing.");
            }
            for entry in found {
                let at = audit::day_of(entry["at_ms"].as_u64().unwrap_or(0));
                let repo = entry["repo"].as_str().map(|repo| format!(" [{repo}]")).unwrap_or_default();
                let ok = match entry["ok"].as_bool() {
                    Some(false) => " (refused)",
                    _ => "",
                };
                println!(
                    "{at}  {:<16} {:<18} {}{repo}{ok}",
                    entry["by"].as_str().unwrap_or("?"),
                    entry["kind"].as_str().unwrap_or("?"),
                    entry["what"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Backup { command } => {
            let mut server = Server::open(&dir_of(cli))?;
            match command.as_ref().unwrap_or(&BackupCmd::Run) {
                BackupCmd::Run => {
                    let done = backup::run(&server.dir, &server.config.backup)?;
                    println!(
                        "{}: {} files, {} bytes, kept {}.",
                        done["name"].as_str().unwrap_or("?"),
                        done["files"],
                        done["size"],
                        done["kept"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" and ")
                    );
                    if let Some(key) = done["new_key"].as_str() {
                        println!(
                            "This server's backup key, made now and shown this once: keep it somewhere else."
                        );
                        println!("A backup cannot be put back without it.\n{key}");
                    }
                }
                BackupCmd::List => {
                    let listed = backup::list(&server.dir, &server.config.backup)?;
                    if listed.is_empty() {
                        println!("No backup yet: `kito-server backup` makes one.");
                    }
                    for one in listed {
                        println!("{}  {:>12} bytes  {}", one.name, one.size, one.kept.join(", "));
                    }
                }
                BackupCmd::Key => {
                    let (key, new) = backup::key(&server.dir)?;
                    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
                    if new {
                        println!("Made now.");
                    }
                    println!("{hex}");
                }
                BackupCmd::Schedule { every, keep, dir } => {
                    server.change(|change| {
                        let backup = &mut change.config_mut().backup;
                        if let Some(every) = every {
                            backup.every_hours = *every;
                        }
                        if let Some(keep) = keep {
                            backup.keep = *keep;
                        }
                        if let Some(dir) = dir {
                            backup.dir = Some(dir.clone());
                        }
                        let what = format!("every {} hours, keep {}", backup.every_hours, backup.keep);
                        change.then_tell("backup_schedule", json!({ "what": what }));
                        Ok::<(), String>(())
                    })?;
                    let config = &server.config.backup;
                    let place = match backup::folder(&server.dir, config) {
                        Some(dir) => dir.display().to_string(),
                        None => "only in S3".to_owned(),
                    };
                    match config.every_hours {
                        0 => println!("Backups only when asked, the last {} kept, in {place}.", config.keep),
                        hours => println!("A backup every {hours} hours while the server serves, the last {} kept, in {place}.", config.keep),
                    }
                }
                BackupCmd::S3 { endpoint, bucket, region, prefix, keys, off } => {
                    if *off {
                        server.change(|change| {
                            change.config_mut().backup.s3 = None;
                            change.then_tell("backup_s3", json!({ "what": "off" }));
                            Ok::<(), String>(())
                        })?;
                        println!("Backups are no longer sent to S3.");
                        return Ok(());
                    }
                    if let Some(keys) = keys {
                        let text =
                            std::fs::read_to_string(keys).map_err(|e| format!("{}: {e}", keys.display()))?;
                        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
                        let (Some(access_key), Some(secret_key)) = (lines.next(), lines.next()) else {
                            return Err(format!(
                                "{}: the access key, then the secret key, each on a line",
                                keys.display()
                            ));
                        };
                        backup::S3::keep_credentials(
                            &server.dir,
                            &backup::Credentials {
                                access_key: access_key.to_owned(),
                                secret_key: secret_key.to_owned(),
                            },
                        )?;
                    }
                    let s3 = backup::S3Config {
                        endpoint: endpoint.clone().unwrap_or_default(),
                        bucket: bucket.clone().unwrap_or_default(),
                        region: region.clone(),
                        prefix: prefix.clone(),
                    };
                    // Asked once, before it is kept: a storage that cannot be reached says so now.
                    backup::S3::new(&server.dir, &s3)?.list()?;
                    server.change(|change| {
                        change.config_mut().backup.s3 = Some(s3.clone());
                        change.then_tell(
                            "backup_s3",
                            json!({ "what": format!("{}/{}", s3.endpoint, s3.bucket) }),
                        );
                        Ok::<(), String>(())
                    })?;
                    println!("Backups go to {}/{} too.", s3.endpoint, s3.bucket);
                }
            }
        }
        Cmd::Restore { archive, to, key } => {
            let server = Server::open(&dir_of(cli)).ok();
            let key = match (key, &server) {
                (Some(file), _) => backup::key_from(file)?,
                (None, Some(server)) => backup::key(&server.dir)?.0,
                (None, None) => return Err("give the backup key: --key <file>".into()),
            };
            let input: Box<dyn std::io::Read> = match archive.strip_prefix("s3:") {
                Some(name) => {
                    let server = server.as_ref().ok_or(
                        "an archive in S3 is fetched by the server it belongs to: run this in its directory",
                    )?;
                    let s3 = server.config.backup.s3.as_ref().ok_or("this server sends nothing to S3")?;
                    backup::S3::new(&server.dir, s3)?.download(name)?
                }
                None => Box::new(std::fs::File::open(archive).map_err(|e| format!("{archive}: {e}"))?),
            };
            let count = backup::restore(input, &key, to)?;
            if let Some(server) = &server {
                audit::record(
                    &server.dir,
                    "backup_restored",
                    None,
                    None,
                    &format!("{archive} into {}", to.display()),
                );
            }
            println!(
                "{count} files put back in {}. Serve from there: `kito-server --dir {} serve`.",
                to.display(),
                to.display()
            );
        }
        Cmd::Serve { listen, plain, http } => {
            let server = Server::open(&dir_of(cli))?;
            // What a server stopped in the middle of a write left behind.
            server.sweep_staged();
            // Backups as the schedule says, while the server serves.
            let dir = server.dir.clone();
            std::thread::spawn(move || loop {
                // Archived repositories kept their time are deleted (ADR 0018).
                if let Ok(mut now) = Server::open(&dir) {
                    // Each told as it is deleted, with the lock held.
                    now.purge_archived();
                }
                let config = Server::open(&dir).map(|server| server.config.backup).unwrap_or_default();
                match backup::due_in(&dir, &config) {
                    Some(wait) if wait.is_zero() => {
                        if let Err(why) = backup::run(&dir, &config) {
                            eprintln!("kito-server: the backup failed: {why}");
                            audit::record(&dir, "backup_failed", None, None, &why);
                            std::thread::sleep(std::time::Duration::from_secs(3600));
                        }
                    }
                    Some(wait) => std::thread::sleep(wait.min(std::time::Duration::from_secs(3600))),
                    None => std::thread::sleep(std::time::Duration::from_secs(3600)),
                }
            });
            if let Some(address) = http.as_deref().or(server.config.http.as_deref()) {
                http::serve(&server, address, *plain)?;
            }
            session::serve(server, listen.as_deref(), *plain)?;
        }
        Cmd::Stdio { user } => {
            let server = Server::open(&dir_of(cli))?;
            let name = user
                .clone()
                .or_else(|| std::env::var("USER").ok())
                .ok_or("who is asking? give --as <user>")?;
            let person: User = server
                .config
                .users
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("{name} is not a user of this server"))?;
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            session::run(&server, &mut stdin.lock(), &mut stdout.lock(), Some((name, person)));
            // The plugins hear of what this client changed before the process leaves.
            state::told();
        }
    }
    Ok(())
}

/// The rules of a repository, after its name on a line.
fn rules_of(protected: bool, checks: &[String], approvals: u32, push: Option<&str>) -> String {
    let mut rules = vec![match protected {
        true => "main protected".to_owned(),
        false => "main open to direct edits".to_owned(),
    }];
    match checks.is_empty() {
        true => rules.push("no checks".to_owned()),
        false => rules.push(format!("checks: {}", checks.join("; "))),
    }
    match approvals {
        0 => {}
        1 => rules.push("1 approval".to_owned()),
        more => rules.push(format!("{more} approvals")),
    }
    if let Some(push) = push {
        rules.push(format!("pushes to {push}"));
    }
    format!("  [{}]", rules.join(", "))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // A command of a plugin: its exit code is the plugin's.
    if let Cmd::External(line) = &cli.command {
        return match run_external(&cli, line) {
            Ok(code) => ExitCode::from(code),
            Err(message) => {
                eprintln!("kito-server: {message}");
                ExitCode::FAILURE
            }
        };
    }
    let done = run(&cli);
    // What the command did is told to the plugins before it leaves.
    state::told();
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("kito-server: {message}");
            ExitCode::FAILURE
        }
    }
}
