//! `kitod`: daemon of a Kito repository.
//!
//! ```text
//! kitod --repo <dir>
//! ```
//!
//! It is not normally launched by hand: the CLI starts it when needed.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut repo: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo" => repo = args.next().map(PathBuf::from),
            "--version" | "-V" => {
                println!("kitod {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("usage: kitod --repo <dir>");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("kitod: unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(repo) = repo else {
        eprintln!("usage: kitod --repo <dir>");
        return ExitCode::from(2);
    };
    // A directory connected to a server has a daemon of another kind: it
    // keeps the lanes worked on from here, and asks the server the rest.
    if kitod::client::Remote::load(&repo).is_some() {
        return match kitod::connected::serve(&repo) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("kitod: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("kitod: starting the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(kitod::server::serve(&repo)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kitod: {e}");
            ExitCode::FAILURE
        }
    }
}
