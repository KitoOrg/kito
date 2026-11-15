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

use clap::{Parser, Subcommand};
use state::{Server, User};
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
    /// Serve the repositories to clients that connect over TCP, with a token
    Serve {
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
    },
    /// List the repositories
    List,
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
    },
    /// List the people and the repositories each may use
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
            println!("Add a repository with `kito-server --dir {} repo add <name>`,", server.dir.display());
            println!("a person with `user add <name>`, then run `serve`.");
        }
        Cmd::Repo { command: RepoCmd::Add { name, from } } => {
            let server = Server::open(&dir_of(cli))?;
            let root = server.add_repo(name, from.as_deref())?;
            println!("Repository `{name}` created in {}", root.display());
        }
        Cmd::Repo { command: RepoCmd::List } => {
            let server = Server::open(&dir_of(cli))?;
            let repos = server.repos();
            if repos.is_empty() {
                println!("No repositories. Add one with `kito-server repo add <name>`.");
            }
            for name in repos {
                println!("{name}");
            }
        }
        Cmd::User { command: UserCmd::Add { name, email, repos } } => {
            let mut server = Server::open(&dir_of(cli))?;
            let token = server.add_user(name, email.clone(), repos)?;
            println!("{name} added. Their token, shown this once:");
            println!("{token}");
        }
        Cmd::User { command: UserCmd::List } => {
            let server = Server::open(&dir_of(cli))?;
            if server.config.users.is_empty() {
                println!("Nobody yet. Add a person with `kito-server user add <name>`.");
            }
            for (name, user) in &server.config.users {
                let email = user.email.as_deref().map(|email| format!(" <{email}>")).unwrap_or_default();
                println!("{name}{email}: {}", user.repos.join(", "));
            }
        }
        Cmd::User { command: UserCmd::Remove { name } } => {
            let mut server = Server::open(&dir_of(cli))?;
            server.remove_user(name)?;
            println!("{name} removed: their token no longer works.");
        }
        Cmd::Serve { listen } => {
            let server = Server::open(&dir_of(cli))?;
            session::serve(server, listen.as_deref())?;
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
