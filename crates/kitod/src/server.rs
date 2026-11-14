//! Il daemon: watcher con debounce, riconciliazione periodica, socket JSON-RPC.

use crate::error::{Error, Result};
use crate::repo::Repo;
use crate::rpc::{self, DaemonStatus, Request, Response, WatcherStatus};
use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Notify};

/// Quante misure di latenza si tengono per i percentili.
const SAMPLES: usize = 1024;
/// Un gruppo di eventi non aspetta la quiete più di tante volte il debounce.
const MAX_WAIT_FACTOR: u32 = 10;

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
    /// Eventi contati dal callback del watcher.
    events: Arc<std::sync::atomic::AtomicU64>,
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
                events: self.events.load(std::sync::atomic::Ordering::Relaxed),
                batches: stats.batches,
                rescans: stats.rescans,
                capture_p50_ms: percentile(&stats.capture_ms, 0.50),
                capture_p95_ms: percentile(&stats.capture_ms, 0.95),
                latency_p50_ms: percentile(&stats.latency_ms, 0.50),
                latency_p95_ms: percentile(&stats.latency_ms, 0.95),
                last_error: stats.last_error.clone(),
            },
            repo,
        })
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
                    let ops = self.with_repo(|repo| repo.scan()).map_err(internal)?;
                    Ok(json!({ "ops": ops.len() }))
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
async fn rescan_loop(daemon: Arc<Daemon>, every: Duration) {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // il primo scatta subito: la scansione iniziale è già fatta
    loop {
        ticker.tick().await;
        let worker = daemon.clone();
        let outcome = tokio::task::spawn_blocking(move || worker.with_repo(|repo| repo.scan())).await;
        daemon.stats.lock().unwrap_or_else(|e| e.into_inner()).rescans += 1;
        match outcome {
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

    // Il watcher parte prima della scansione iniziale: così nessuna modifica
    // può cadere nell'intervallo tra le due.
    let (tx, rx) = mpsc::unbounded_channel::<WatchMsg>();
    let watch_root = root.clone();
    let stats_events = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter = stats_events.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let msg = match event {
            Ok(event) if event.need_rescan() => WatchMsg::Rescan("il watcher ha perso degli eventi".into()),
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
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

    repo.scan()?;

    let socket = rpc::socket_path(&root);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).map_err(Error::io(&socket))?;

    let daemon = Arc::new(Daemon {
        root: root.clone(),
        socket: socket.clone(),
        repo: Mutex::new(Some(repo)),
        stats: Mutex::new(WatchStats::default()),
        events: stats_events,
        started: Instant::now(),
        debounce,
        shutdown: Notify::new(),
    });
    eprintln!("kitod: in ascolto su {} per {}", socket.display(), daemon.root.display());

    let debouncer = tokio::spawn(debounce_loop(daemon.clone(), rx));
    let rescanner = tokio::spawn(rescan_loop(daemon.clone(), rescan_every));
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
