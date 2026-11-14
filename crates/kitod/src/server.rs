//! Il daemon: watcher con debounce, riconciliazione periodica, socket JSON-RPC.

use crate::error::{Error, Result};
use crate::repo::Repo;
use crate::rpc::{self, DaemonStatus, Request, Response, WatcherStatus};
use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Notify};

/// Quante misure di latenza si tengono per i percentili.
const SAMPLES: usize = 1024;
/// Un gruppo di eventi non aspetta la quiete più di tante volte il debounce.
const MAX_WAIT_FACTOR: u32 = 10;
/// Quando gli eventi non sono affidabili si scansiona di continuo, ma
/// spendendo in scansioni al più un ventesimo del tempo…
const FAST_SCAN_FACTOR: u32 = 20;
/// …e mai più di una volta al secondo.
const FAST_SCAN_MIN: Duration = Duration::from_secs(1);
/// Una scansione che trova modifiche quando il watcher tace da così tanto
/// ha trovato modifiche che il watcher non ha segnalato.
const QUIET: Duration = Duration::from_secs(1);

/// Contatori aggiornati dal callback del watcher.
#[derive(Default)]
struct Signals {
    /// Eventi ricevuti (esclusi quelli interni).
    events: AtomicU64,
    /// Millisecondi dall'avvio del daemon all'ultimo evento.
    last_event_ms: AtomicU64,
}

#[derive(Default)]
struct WatchStats {
    batches: u64,
    rescans: u64,
    capture_ms: VecDeque<f64>,
    latency_ms: VecDeque<f64>,
    last_error: Option<String>,
}

fn percentile(samples: &VecDeque<f64>, p: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = samples.iter().copied().collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let rank = ((sorted.len() as f64) * p).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

struct Daemon {
    root: PathBuf,
    socket: PathBuf,
    /// `None` dopo lo spegnimento.
    repo: Mutex<Option<Repo>>,
    stats: Mutex<WatchStats>,
    signals: Arc<Signals>,
    /// `false` se il watcher è disattivato dalla configurazione.
    watching: bool,
    filesystem: Option<String>,
    /// Gli eventi non sono affidabili: scansione frequente.
    fast: AtomicBool,
    fast_changed: Notify,
    /// Modifiche trovate dalla scansione e mai segnalate dal watcher.
    missed: AtomicU64,
    /// Durata dell'ultima scansione, in microsecondi.
    last_scan_us: AtomicU64,
    rescan_every: Duration,
    started: Instant,
    debounce: Duration,
    shutdown: Notify,
}

impl Daemon {
    fn with_repo<T>(&self, f: impl FnOnce(&mut Repo) -> Result<T>) -> Result<T> {
        let mut guard = self.repo.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(repo) => f(repo),
            None => Err(Error::Other("il daemon si sta spegnendo".into())),
        }
    }

    fn status(&self) -> Result<DaemonStatus> {
        let repo = self.with_repo(|repo| repo.status())?;
        let stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        Ok(DaemonStatus {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime_secs: self.started.elapsed().as_secs(),
            socket: self.socket.clone(),
            debounce_ms: self.debounce.as_millis() as u64,
            watcher: WatcherStatus {
                events: self.signals.events.load(Ordering::Relaxed),
                batches: stats.batches,
                rescans: stats.rescans,
                watching: self.watching,
                filesystem: self.filesystem.clone(),
                events_unreliable: self.fast.load(Ordering::Relaxed),
                missed_by_watcher: self.missed.load(Ordering::Relaxed),
                rescan_interval_ms: self.rescan_interval().as_millis() as u64,
                capture_p50_ms: percentile(&stats.capture_ms, 0.50),
                capture_p95_ms: percentile(&stats.capture_ms, 0.95),
                latency_p50_ms: percentile(&stats.latency_ms, 0.50),
                latency_p95_ms: percentile(&stats.latency_ms, 0.95),
                last_error: stats.last_error.clone(),
            },
            repo,
        })
    }

    /// Tempo trascorso dall'ultimo evento del watcher (`None`: mai arrivato uno).
    fn since_last_event(&self) -> Option<Duration> {
        if self.signals.events.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let last = Duration::from_millis(self.signals.last_event_ms.load(Ordering::Relaxed));
        Some(self.started.elapsed().saturating_sub(last))
    }

    /// Scansione completa di riconciliazione. Se trova modifiche mentre il
    /// watcher tace, vuol dire che qui gli eventi non arrivano tutti: da quel
    /// momento il daemon scansiona spesso invece di fidarsi del watcher.
    fn reconcile(&self) -> Result<usize> {
        let quiet = self.since_last_event().is_none_or(|t| t >= QUIET);
        let start = Instant::now();
        let ops = self.with_repo(|repo| repo.scan())?;
        self.last_scan_us.store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).rescans += 1;
        if !ops.is_empty() && quiet && self.watching {
            self.missed.fetch_add(ops.len() as u64, Ordering::Relaxed);
            if !self.fast.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "kitod: {} modifiche non segnalate dal watcher: da ora la cartella viene scansionata spesso",
                    ops.len()
                );
                self.fast_changed.notify_one();
            }
        }
        Ok(ops.len())
    }

    /// Intervallo tra due scansioni periodiche.
    fn rescan_interval(&self) -> Duration {
        if !self.fast.load(Ordering::Relaxed) {
            return self.rescan_every;
        }
        let last_scan = Duration::from_micros(self.last_scan_us.load(Ordering::Relaxed));
        (last_scan * FAST_SCAN_FACTOR).max(FAST_SCAN_MIN).min(self.rescan_every)
    }

    /// Esegue una richiesta. È codice bloccante: va chiamato fuori dal runtime.
    fn handle(&self, request: &Request) -> Response {
        let id = request.id;
        let params = &request.params;
        let result: std::result::Result<Value, (i64, String)> = (|| {
            let internal = |e: Error| (rpc::INTERNAL_ERROR, e.to_string());
            let to_json = |v: std::result::Result<Value, serde_json::Error>| {
                v.map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))
            };
            match request.method.as_str() {
                "status" => to_json(serde_json::to_value(self.status().map_err(internal)?)),
                "sync" => {
                    let ops = self.reconcile().map_err(internal)?;
                    Ok(json!({ "ops": ops }))
                }
                "log" => {
                    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                    let path = params.get("path").and_then(Value::as_str).map(PathBuf::from);
                    let items = self.with_repo(|repo| repo.log(limit, path.as_deref())).map_err(internal)?;
                    to_json(serde_json::to_value(items))
                }
                "checkpoint" => {
                    let label = params
                        .get("label")
                        .and_then(Value::as_str)
                        .ok_or((rpc::INVALID_PARAMS, "manca il parametro `label`".to_owned()))?;
                    let done = self.with_repo(|repo| repo.checkpoint(label)).map_err(internal)?;
                    to_json(serde_json::to_value(done))
                }
                "export_git" => {
                    let done = self.with_repo(|repo| repo.export_git()).map_err(internal)?;
                    let list: Vec<Value> =
                        done.into_iter().map(|(c, g)| json!({ "checkpoint": c, "commit": g })).collect();
                    Ok(Value::Array(list))
                }
                "shutdown" => {
                    self.shutdown.notify_one();
                    Ok(json!({}))
                }
                other => Err((rpc::METHOD_NOT_FOUND, format!("metodo sconosciuto: {other}"))),
            }
        })();
        match result {
            Ok(value) => Response::ok(id, value),
            Err((code, message)) => Response::err(id, code, message),
        }
    }

    fn record(&self, capture: Duration, latency: Option<Duration>, error: Option<String>) {
        let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        let push = |queue: &mut VecDeque<f64>, d: Duration| {
            if queue.len() == SAMPLES {
                queue.pop_front();
            }
            queue.push_back(d.as_secs_f64() * 1000.0);
        };
        push(&mut stats.capture_ms, capture);
        if let Some(latency) = latency {
            push(&mut stats.latency_ms, latency);
        }
        if error.is_some() {
            stats.last_error = error;
        }
    }
}

/// `true` per i percorsi che il daemon stesso scrive o che non sono mai
/// tracciati: non vale la pena nemmeno svegliare la cattura.
fn is_internal(rel: &Path) -> bool {
    let mut parts = rel.components();
    match parts.next() {
        Some(Component::Normal(first)) if first == ".kito" => true,
        Some(Component::Normal(first)) if first == ".git" => true,
        _ => rel.components().any(|c| c.as_os_str() == ".git"),
    }
}

enum WatchMsg {
    Paths(Vec<PathBuf>),
    Rescan(String),
}

/// Raccoglie gli eventi del watcher e li passa alla cattura dopo un periodo
/// di quiete (debounce).
async fn debounce_loop(daemon: Arc<Daemon>, mut events: mpsc::UnboundedReceiver<WatchMsg>) {
    let max_wait = daemon.debounce * MAX_WAIT_FACTOR;
    while let Some(first) = events.recv().await {
        let batch_start = Instant::now();
        let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
        let mut rescan: Option<String> = None;
        let mut absorb = |msg: WatchMsg| match msg {
            WatchMsg::Paths(list) => paths.extend(list),
            WatchMsg::Rescan(why) => rescan = Some(why),
        };
        absorb(first);
        loop {
            let left = max_wait.saturating_sub(batch_start.elapsed());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(daemon.debounce.min(left), events.recv()).await {
                Ok(Some(msg)) => absorb(msg),
                Ok(None) | Err(_) => break,
            }
        }

        let worker = daemon.clone();
        let capture_start = Instant::now();
        let needs_rescan = rescan.is_some();
        let outcome = tokio::task::spawn_blocking(move || {
            worker.with_repo(|repo| if needs_rescan { repo.scan() } else { repo.capture(paths) })
        })
        .await;
        let (captured, error) = match outcome {
            Ok(Ok(ops)) => (!ops.is_empty(), rescan),
            Ok(Err(e)) => (false, Some(e.to_string())),
            Err(e) => (false, Some(e.to_string())),
        };
        {
            let mut stats = daemon.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.batches += 1;
            stats.rescans += u64::from(needs_rescan);
        }
        if let Some(message) = &error {
            eprintln!("kitod: cattura: {message}");
        }
        // La latenza si misura solo sui gruppi che hanno prodotto op.
        daemon.record(capture_start.elapsed(), captured.then(|| batch_start.elapsed()), error);
    }
}

/// Scansione periodica: recupera gli eventi che il watcher ha perso.
async fn rescan_loop(daemon: Arc<Daemon>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(daemon.rescan_interval()) => {}
            // È appena scattata la scansione frequente: l'attesa si ricalcola.
            _ = daemon.fast_changed.notified() => continue,
        }
        let worker = daemon.clone();
        match tokio::task::spawn_blocking(move || worker.reconcile()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("kitod: scansione periodica: {e}"),
            Err(_) => return,
        }
    }
}

async fn serve_connection(daemon: Arc<Daemon>, stream: UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let worker = daemon.clone();
                let id = request.id;
                tokio::task::spawn_blocking(move || worker.handle(&request))
                    .await
                    .unwrap_or_else(|e| Response::err(id, rpc::INTERNAL_ERROR, e.to_string()))
            }
            Err(e) => Response::err(0, rpc::PARSE_ERROR, format!("richiesta non valida: {e}")),
        };
        let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
        bytes.push(b'\n');
        if writer.write_all(&bytes).await.is_err() {
            break;
        }
    }
}

/// Esegue il daemon del repository in `root` fino allo spegnimento
/// (richiesta `shutdown`, SIGTERM o Ctrl-C).
pub async fn serve(root: &Path) -> Result<()> {
    // Aprire lo store prende il lock esclusivo: un solo daemon per repository.
    let mut repo = Repo::open(root)?;
    let root = repo.root().to_owned();
    let debounce = Duration::from_millis(repo.config().capture.debounce_ms.max(1));
    let rescan_every = Duration::from_secs(repo.config().capture.rescan_secs.max(1));

    let watching = repo.config().capture.watch;
    let filesystem = crate::fsinfo::filesystem_type(&root);
    let unreliable = filesystem.as_deref().is_some_and(crate::fsinfo::events_are_unreliable);
    let started = Instant::now();

    // Il watcher parte prima della scansione iniziale: così nessuna modifica
    // può cadere nell'intervallo tra le due.
    let (tx, rx) = mpsc::unbounded_channel::<WatchMsg>();
    let signals = Arc::new(Signals::default());
    let watcher = if watching {
        let watch_root = root.clone();
        let seen = signals.clone();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let msg = match event {
                Ok(event) if event.need_rescan() => {
                    WatchMsg::Rescan("il watcher ha perso degli eventi".into())
                }
                Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => return,
                Ok(event) => {
                    let paths: Vec<PathBuf> = event
                        .paths
                        .iter()
                        .filter_map(|p| p.strip_prefix(&watch_root).ok())
                        .filter(|rel| !rel.as_os_str().is_empty() && !is_internal(rel))
                        .map(Path::to_owned)
                        .collect();
                    if paths.is_empty() {
                        return;
                    }
                    seen.last_event_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    seen.events.fetch_add(1, Ordering::Relaxed);
                    WatchMsg::Paths(paths)
                }
                Err(e) => WatchMsg::Rescan(format!("errore del watcher: {e}")),
            };
            let _ = tx.send(msg);
        })
        .map_err(|e| Error::Other(format!("avvio del watcher: {e}")))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| Error::Other(format!("watcher su {}: {e}", root.display())))?;
        Some(watcher)
    } else {
        drop(tx);
        None
    };

    repo.scan()?;

    let socket = rpc::socket_path(&root);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).map_err(Error::io(&socket))?;

    let daemon = Arc::new(Daemon {
        root: root.clone(),
        socket: socket.clone(),
        repo: Mutex::new(Some(repo)),
        stats: Mutex::new(WatchStats::default()),
        signals,
        watching,
        filesystem,
        fast: AtomicBool::new(!watching || unreliable),
        fast_changed: Notify::new(),
        missed: AtomicU64::new(0),
        last_scan_us: AtomicU64::new(0),
        rescan_every,
        started,
        debounce,
        shutdown: Notify::new(),
    });
    eprintln!("kitod: in ascolto su {} per {}", socket.display(), daemon.root.display());
    if !watching {
        eprintln!("kitod: watcher disattivato dalla configurazione: la cartella viene scansionata spesso");
    } else if unreliable {
        eprintln!(
            "kitod: file system {}: gli eventi non sono affidabili, la cartella viene scansionata spesso",
            daemon.filesystem.as_deref().unwrap_or("?")
        );
    }

    let debouncer = tokio::spawn(debounce_loop(daemon.clone(), rx));
    let rescanner = tokio::spawn(rescan_loop(daemon.clone()));
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| Error::Other(format!("gestione dei segnali: {e}")))?;

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(serve_connection(daemon.clone(), stream));
                }
                Err(e) => eprintln!("kitod: accept: {e}"),
            },
            _ = daemon.shutdown.notified() => break,
            _ = sigterm.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    // Da qui nessuna nuova connessione viene accettata. Il file del socket
    // resta finché lo store non è chiuso: chi aspetta lo spegnimento
    // (`kito daemon stop`) sa che quando sparisce il lock è libero.
    drop(listener);
    drop(watcher);
    debouncer.abort();
    rescanner.abort();
    // Ultima scansione e chiusura pulita: stato salvato, indici durevoli.
    let repo = daemon.repo.lock().unwrap_or_else(|e| e.into_inner()).take();
    let closed = match repo {
        Some(mut repo) => tokio::task::spawn_blocking(move || {
            repo.scan()?;
            repo.close()
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))
        .and_then(|r| r),
        None => Ok(()),
    };
    let _ = std::fs::remove_file(&socket);
    closed?;
    eprintln!("kitod: spento");
    Ok(())
}
