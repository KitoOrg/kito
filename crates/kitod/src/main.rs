//! `kitod`: daemon di un repository Kito.
//!
//! ```text
//! kitod --repo <cartella>
//! ```
//!
//! Di norma non si lancia a mano: lo avvia la CLI quando serve.

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
                println!("uso: kitod --repo <cartella>");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("kitod: argomento sconosciuto: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(repo) = repo else {
        eprintln!("uso: kitod --repo <cartella>");
        return ExitCode::from(2);
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("kitod: avvio del runtime: {e}");
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
