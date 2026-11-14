//! `kito`: la riga di comando per gli umani.
//!
//! È un client sottile del daemon (`kitod`), che avvia da sé se non è già
//! attivo. Solo `init` e `import` lavorano direttamente sullo store, perché
//! servono prima che un daemon abbia senso.

use clap::{Parser, Subcommand};
use kitod::client::{find_root, Client, ClientError};
use kitod::identity::{self, Identity};
use kitod::repo::{CheckpointResult, LogItem, RollbackResult};
use kitod::rpc::DaemonStatus;
use kitod::Repo;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "kito", version, about = "Version control semantico per sciami di agenti")]
struct Cli {
    /// Radice del repository (default: si cerca `.kito/` dalla cartella corrente in su).
    #[arg(long, global = true, value_name = "CARTELLA")]
    repo: Option<PathBuf>,
    /// Stampa lo stesso output strutturato del server MCP.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Crea `.kito/` in una cartella e comincia a registrare i salvataggi
    Init {
        /// Cartella da inizializzare (default: quella corrente)
        path: Option<PathBuf>,
        /// Parte da zero anche se la cartella è un repository git
        #[arg(long)]
        fresh: bool,
    },
    /// Importa lo stato attuale e la storia di un repo git
    Import {
        /// Repository git da importare (default: la cartella corrente)
        path: Option<PathBuf>,
    },
    /// Mostra il log delle operazioni, dalla più recente
    Log {
        /// Solo le operazioni che toccano questo file
        path: Option<PathBuf>,
        /// Quante operazioni mostrare
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
    },
    /// Crea un checkpoint a mano
    Checkpoint {
        /// Etichetta del checkpoint
        #[arg(short = 'm', long = "message", value_name = "ETICHETTA")]
        label: String,
    },
    /// Esporta su git i checkpoint non ancora esportati
    ExportGit,
    /// Riporta i file allo stato di un checkpoint o di un'operazione precedente
    Rollback {
        /// Punto a cui tornare: un checkpoint (ckpt#3) o le prime cifre
        /// dell'ID di un'operazione, come li mostra `kito log`
        to: Option<String>,
        /// Solo questi file o cartelle (default: tutto)
        paths: Vec<PathBuf>,
        /// Torna a prima delle ultime N operazioni
        #[arg(long, value_name = "N", conflicts_with = "to")]
        ops: Option<u64>,
        /// Mostra cosa cambierebbe, senza toccare nulla
        #[arg(long)]
        dry_run: bool,
    },
    /// Mostra o imposta chi firma le modifiche (nome ed email)
    Identity {
        #[arg(long)]
        name: Option<String>,
        /// Usata per attribuire i commit esportati su git
        #[arg(long)]
        email: Option<String>,
        /// Le modifiche sono di un agente
        #[arg(long, conflicts_with = "human")]
        agent: bool,
        /// Le modifiche sono di una persona
        #[arg(long)]
        human: bool,
        /// Vale per tutti i tuoi repository (configurazione personale)
        #[arg(long)]
        global: bool,
    },
    /// Attende che tutti i salvataggi siano registrati
    Sync,
    /// Stato del repository
    Status,
    /// Gestione del daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Stato del daemon e del watcher
    Status,
    /// Avvia il daemon se non è attivo
    Start,
    /// Ferma il daemon
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

/// Data e ora UTC da millisecondi Unix (`2026-11-14 12:31:07`).
fn utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Algoritmo di Howard Hinnant: giorni dall'epoca → data civile.
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

/// Ferma il daemon se è attivo e attende che abbia chiuso lo store.
fn stop_daemon(root: &Path) -> Result<bool, String> {
    let Ok(mut client) = Client::connect(root) else {
        return Ok(false);
    };
    let _: Value = client.call("shutdown", json!({})).map_err(fail)?;
    // Il file del socket sparisce solo dopo la chiusura dello store.
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
        return Err(
            "questa cartella è un repository git: usa `kito import` per portare con te la sua storia, \
                    oppure `kito init --fresh` per partire da zero"
                .to_owned(),
        );
    }
    let repo = Repo::init(&root).map_err(fail)?;
    let root = repo.root().to_owned();
    repo.close().map_err(fail)?;
    // La cattura è continua da subito: il daemon parte con il repository.
    let mut client = Client::connect_or_start(&root).map_err(fail)?;
    if cli.json {
        print_json(&json!({ "root": root }));
    } else {
        println!("Repository Kito creato in {}", root.join(".kito").display());
        println!("Il daemon è attivo: da ora ogni salvataggio viene registrato.");
        warn_if_events_unreliable(&mut client);
    }
    Ok(())
}

fn cmd_import(cli: &Cli, path: &Option<PathBuf>) -> Outcome {
    let root = path.clone().or_else(|| cli.repo.clone()).unwrap_or_else(|| PathBuf::from("."));
    let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
    // L'import lavora direttamente sullo store: il daemon, se c'è, si fa da parte.
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
            "Importati {} commit: {} operazioni, {} checkpoint.",
            report.commits, report.ops, report.checkpoints
        );
        for line in &report.skipped {
            println!("  saltato {line}");
        }
        if !captured.is_empty() {
            println!("Catturate {} modifiche non ancora presenti in git.", captured.len());
        }
        println!("Il daemon è attivo: da ora ogni salvataggio viene registrato.");
        warn_if_events_unreliable(&mut client);
    }
    Ok(())
}

fn cmd_log(cli: &Cli, path: &Option<PathBuf>, limit: usize) -> Outcome {
    let root = root_of(cli)?;
    let rel = path.as_ref().map(|p| repo_relative(&root, p)).transpose()?;
    let mut client = daemon(cli)?;
    // Prima di mostrare il log ci si assicura che contenga tutto ciò che è
    // su disco, anche se il watcher avesse perso qualche evento.
    client.call_value("sync", json!({})).map_err(fail)?;
    let items: Vec<LogItem> = client.call("log", json!({ "limit": limit, "path": rel })).map_err(fail)?;
    if cli.json {
        print_json(&items);
        return Ok(());
    }
    if items.is_empty() {
        println!("Nessuna operazione registrata.");
    }
    for item in items {
        match item {
            LogItem::Checkpoint { id, label, git_commit, .. } => {
                let git = git_commit.map(|c| format!("  (git {})", &c[..10])).unwrap_or_default();
                println!("◆ {id}  {label}{git}");
            }
            LogItem::Op {
                id, at_ms, author, op, path, detail, lines_added, lines_removed, intent, ..
            } => {
                // Le prime cifre dell'ID bastano per indicare l'op a `kito rollback`.
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
            }
        }
    }
    Ok(())
}

/// Un percorso indicato come lo si vede dalla cartella corrente, reso
/// relativo alla radice del repository.
fn repo_relative(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let abs = std::env::current_dir().map_err(fail)?.join(path);
    // Il file può non esistere più: si risolve la cartella che lo contiene.
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
            return Err("indica a che punto tornare: un checkpoint (ckpt#3), le prime cifre dell'ID di \
                        un'operazione (vedi `kito log`), oppure --ops N"
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
        println!("I file sono già nello stato di {}: niente da fare.", done.target);
        return Ok(());
    }
    let files = match done.changes.len() {
        1 => "1 file".to_owned(),
        n => format!("{n} file"),
    };
    if done.applied {
        println!("Rollback a {}: {files}", done.target);
    } else {
        println!("Il rollback a {} cambierebbe {files} (prova a vuoto, nulla è stato toccato):", done.target);
    }
    for change in &done.changes {
        println!("  {:<10} {}", change.action, change.path);
    }
    if let (true, Some(undo)) = (done.applied, &done.undo) {
        println!("Registrato come {} operazioni: la storia precedente resta intatta.", done.ops);
        println!("Per annullarlo: kito rollback {undo}");
    }
    Ok(())
}

fn print_identity(identity: &Identity) {
    let kind = if identity.kind == "agent" { "agente" } else { "persona" };
    println!("{} ({kind})", identity.author());
    println!("Fonte: {}", identity.source.describe());
    if identity.email.is_none() {
        println!("Nessuna email: i commit esportati su git useranno un indirizzo segnaposto.");
        println!("Per impostarla: kito identity --email tu@esempio.it");
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
            identity::personal_config_path()
                .ok_or("impossibile trovare la cartella di configurazione personale")?
        } else {
            match &root {
                Some(root) => root.join(".kito").join("config.toml"),
                None => return Err(format!("{} (oppure usa --global)", ClientError::NoRepository)),
            }
        };
        identity::write(&path, name.as_deref(), email.as_deref(), kind)?;
    }
    let identity: Identity = match &root {
        // Il daemon rilegge la configurazione: l'identità vale dalla prossima op.
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
        (Some(commit), _) => println!("{} creato ed esportato come commit git {}", done.id, &commit[..10]),
        (None, Some(error)) => println!("{} creato; export su git non riuscito: {error}", done.id),
        (None, None) => println!("{} creato", done.id),
    }
    Ok(())
}

fn cmd_export(cli: &Cli) -> Outcome {
    let done: Vec<Value> = daemon(cli)?.call("export_git", json!({})).map_err(fail)?;
    if cli.json {
        print_json(&done);
    } else if done.is_empty() {
        println!("Niente da esportare.");
    } else {
        for item in done {
            let commit = item["commit"].as_str().unwrap_or_default();
            println!(
                "{} → commit git {}",
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
        println!("Tutto registrato ({} nuove operazioni).", done["ops"]);
    }
    Ok(())
}

fn print_status(status: &DaemonStatus, with_daemon: bool) {
    let repo = &status.repo;
    println!("Repository ........ {}", repo.root.display());
    println!("Autore ............ {}", repo.author);
    println!("File tracciati .... {}", repo.files);
    println!("Operazioni ........ {} ({} dopo l'ultimo checkpoint)", repo.ops, repo.ops_since_checkpoint);
    println!(
        "Checkpoint ........ {}{}",
        repo.checkpoints,
        match (repo.last_checkpoint.as_deref(), repo.unexported) {
            (Some(last), 0) => format!(" (ultimo {last}, tutti esportati su git)"),
            (Some(last), n) => format!(" (ultimo {last}, {n} da esportare su git)"),
            (None, _) => String::new(),
        }
    );
    if with_daemon {
        let w = &status.watcher;
        let ms = |v: Option<f64>| v.map_or("—".to_owned(), |v| format!("{v:.1} ms"));
        println!(
            "Daemon ............ pid {}, attivo da {} s, versione {}",
            status.pid, status.uptime_secs, status.version
        );
        println!("Socket ............ {}", status.socket.display());
        println!(
            "Watcher ........... {} eventi, {} gruppi, {} scansioni complete",
            w.events, w.batches, w.rescans
        );
        println!(
            "Cattura ........... p50 {}, p95 {} (dopo il debounce di {} ms)",
            ms(w.capture_p50_ms),
            ms(w.capture_p95_ms),
            status.debounce_ms
        );
        println!(
            "Salvataggio → op .. p50 {}, p95 {} (debounce compreso)",
            ms(w.latency_p50_ms),
            ms(w.latency_p95_ms)
        );
        if let Some(error) = &w.last_error {
            println!("Ultimo avviso ..... {error}");
        }
        println!("Riconciliazione ... scansione completa ogni {}", seconds(w.rescan_interval_ms));
        if let Some(why) = unreliable_events(status) {
            println!("                    {why}");
        }
        println!("Language server ... nessuno (arrivano con la fase 4)");
    }
}

fn seconds(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// Se in questo repository gli eventi dei file non bastano, spiega perché e
/// cosa fa Kito al loro posto.
fn unreliable_events(status: &DaemonStatus) -> Option<String> {
    let w = &status.watcher;
    if !w.events_unreliable {
        return None;
    }
    let cause = if !w.watching {
        "il watcher è disattivato in .kito/config.toml".to_owned()
    } else if w.missed_by_watcher > 0 {
        format!("{} modifiche non sono state segnalate dal sistema operativo", w.missed_by_watcher)
    } else {
        format!(
            "sul file system {} gli eventi dei file non sono affidabili",
            w.filesystem.as_deref().unwrap_or("di questa cartella")
        )
    };
    Some(format!(
        "{cause}: le modifiche vengono trovate dalla scansione, entro {} circa",
        seconds(w.rescan_interval_ms)
    ))
}

/// Dopo `init` e `import`: avvisa se qui la cattura non sarà immediata.
fn warn_if_events_unreliable(client: &mut Client) {
    let Ok(status) = client.call::<DaemonStatus>("status", json!({})) else { return };
    if let Some(why) = unreliable_events(&status) {
        println!("Nota: {why}.");
        if status.watcher.filesystem.as_deref() == Some("9p") {
            println!("      Sotto WSL conviene tenere il progetto nel file system di Linux (per esempio in ~/progetti).");
        }
    }
}

fn cmd_status(cli: &Cli, with_daemon: bool) -> Outcome {
    let mut client = daemon(cli)?;
    if !with_daemon {
        // Lo stato del repository deve riflettere ciò che è su disco adesso.
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
        println!("Daemon fermato.");
    } else {
        println!("Il daemon non era attivo.");
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
        Cmd::Sync => cmd_sync(&cli),
        Cmd::Status => cmd_status(&cli, false),
        Cmd::Daemon { command: DaemonCmd::Status } => cmd_status(&cli, true),
        Cmd::Daemon { command: DaemonCmd::Start } => daemon(&cli).map(|_| {
            if !cli.json {
                println!("Daemon attivo.");
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
