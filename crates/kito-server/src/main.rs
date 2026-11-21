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

mod session;
mod state;
mod tls;

use clap::{Parser, Subcommand};
use kitod::client::Client;
use serde_json::{json, Value};
use state::{Role, Server, User};
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
enum Cmd {
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
    /// Add a repository, empty or with the history of a git repository
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
    },
}

#[derive(Subcommand)]
enum UserCmd {
    /// Add a person and print their token, once
    Add {
        name: String,
        #[arg(long)]
        email: Option<String>,
        /// A repository the person may use; once per repository (default: all)
        #[arg(long = "repo", value_name = "NAME")]
        repos: Vec<String>,
        /// What they may do: read (see and follow), write (open lanes and
        /// work in them; their integrations wait for approval), integrate
        #[arg(long, value_enum, default_value = "integrate")]
        role: Role,
    },
    /// Change what a person may do: everywhere, or in one repository
    Role {
        name: String,
        #[arg(value_enum)]
        role: Role,
        /// Only in this repository
        #[arg(long, value_name = "NAME")]
        repo: Option<String>,
    },
    /// List the people, the repositories each may use and what they may do
    List,
    /// Remove a person: their token stops working
    Remove { name: String },
}

fn dir_of(cli: &Cli) -> PathBuf {
    cli.dir
        .clone()
        .or_else(|| std::env::var_os("KITO_SERVER_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn run(cli: &Cli) -> Result<(), String> {
    match &cli.command {
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
            let root = server.add_repo(name, from.as_deref(), !unprotected)?;
            println!("Repository `{name}` created in {}", root.display());
            match (from, unprotected) {
                (Some(_), false) => {
                    println!("Its main lane is protected: it changes only when a lane is integrated.")
                }
                (Some(_), true) => {}
                (None, _) => println!(
                    "It is empty: put its files there, `kito checkpoint`, then `kito-server repo set {name} --protect`."
                ),
            }
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
            command: RepoCmd::Set { name, protect, unprotect, checks, no_checks, approvals, push, no_push },
        } => {
            let server = Server::open(&dir_of(cli))?;
            let root =
                server.repo(name).ok_or_else(|| format!("there is no repository `{name}` on this server"))?;
            // Through the repository's daemon, which holds the rules it applies.
            let mut daemon = Client::connect_or_start(&root).map_err(|e| e.to_string())?;
            if *protect || *unprotect {
                let _: Value =
                    daemon.call("protect", json!({ "on": *protect })).map_err(|e| e.to_string())?;
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
            let now: Value = daemon.call("configure", rules).map_err(|e| e.to_string())?;
            let checks: Vec<String> = serde_json::from_value(now["checks"].clone()).unwrap_or_default();
            let rules = rules_of(
                now["protected"].as_bool().unwrap_or(false),
                &checks,
                now["approvals"].as_u64().unwrap_or(0) as u32,
                now["push"].as_str(),
            );
            println!("{name}{rules}");
        }
        Cmd::User { command: UserCmd::Add { name, email, repos, role } } => {
            let mut server = Server::open(&dir_of(cli))?;
            let token = server.add_user(name, email.clone(), repos, *role)?;
            println!("{name} added. Their token, shown this once:");
            println!("{token}");
            // What they connect with: the token, and what says the server
            // they reach is this one.
            println!("They connect with:");
            println!(
                "  kito connect <host>:<port> <repository> --token {token} --fingerprint {}",
                tls::fingerprint(&server.dir)?
            );
        }
        Cmd::User { command: UserCmd::Role { name, role, repo } } => {
            let mut server = Server::open(&dir_of(cli))?;
            server.set_role(name, *role, repo.as_deref())?;
            match repo {
                Some(repo) => println!("{name} may {} in `{repo}`.", role.word()),
                None => println!("{name} may {}.", role.word()),
            }
        }
        Cmd::User { command: UserCmd::List } => {
            let server = Server::open(&dir_of(cli))?;
            if server.config.users.is_empty() {
                println!("Nobody yet. Add a person with `kito-server user add <name>`.");
            }
            for (name, user) in &server.config.users {
                let email = user.email.as_deref().map(|email| format!(" <{email}>")).unwrap_or_default();
                // What they may do where: the repositories they may use,
                // then the ones where it is something else.
                let mut places: Vec<String> =
                    user.repos.iter().map(|repo| format!("{repo} ({})", user.role_in(repo).word())).collect();
                let named = user.roles.iter().filter(|(repo, _)| !user.repos.contains(*repo));
                places.extend(named.map(|(repo, role)| format!("{repo} ({})", role.word())));
                println!("{name}{email}: {}", places.join(", "));
            }
        }
        Cmd::User { command: UserCmd::Remove { name } } => {
            let mut server = Server::open(&dir_of(cli))?;
            server.remove_user(name)?;
            println!("{name} removed: their token no longer works.");
        }
        Cmd::Fingerprint => {
            let server = Server::open(&dir_of(cli))?;
            println!("{}", tls::fingerprint(&server.dir)?);
        }
        Cmd::Serve { listen, plain } => {
            let server = Server::open(&dir_of(cli))?;
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
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("kito-server: {message}");
            ExitCode::FAILURE
        }
    }
}
