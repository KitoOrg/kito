//! The daemon: watcher with debounce, periodic reconciliation, JSON-RPC socket.

use crate::error::{Error, Result};
use crate::integrate::{Decision, Preparation};
use crate::repo::Repo;
use crate::rpc::{self, DaemonStatus, Request, Response, WatcherStatus};
use kito_core::{ConflictId, LaneId};
use kito_integrate::{IntegrationResult, JobId, JobState, Queue};
use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Notify};

/// How many latency samples are kept for the percentiles.
const SAMPLES: usize = 1024;
/// A batch of events does not wait for quiet longer than this many times the debounce.
const MAX_WAIT_FACTOR: u32 = 10;
/// When events are unreliable the daemon scans continuously, but spending
/// at most a twentieth of its time on scans…
const FAST_SCAN_FACTOR: u32 = 20;
/// …and never more than once per second.
const FAST_SCAN_MIN: Duration = Duration::from_secs(1);
/// A scan that finds changes when the watcher has been silent for this long
/// has found changes the watcher did not report.
const QUIET: Duration = Duration::from_secs(1);

/// `true` where the system reports when a program closes a file it was
/// writing (inotify). Elsewhere a file that was written to is simply given
/// a moment to settle.
const CLOSE_IS_REPORTED: bool = cfg!(target_os = "linux");

/// Counters updated by the watcher callback.
#[derive(Default)]
struct Signals {
    /// Events received (internal ones excluded).
    events: AtomicU64,
    /// Milliseconds from the start of the daemon to the last event.
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
    /// `None` after shutdown.
    repo: Mutex<Option<Repo>>,
    stats: Mutex<WatchStats>,
    signals: Arc<Signals>,
    /// `false` if the watcher is disabled by the configuration.
    watching: bool,
    filesystem: Option<String>,
    /// Events are unreliable: frequent scanning.
    fast: AtomicBool,
    fast_changed: Notify,
    /// Changes found by the scan and never reported by the watcher.
    missed: AtomicU64,
    /// Duration of the last scan, in microseconds.
    last_scan_us: AtomicU64,
    rescan_every: Duration,
    started: Instant,
    debounce: Duration,
    shutdown: Notify,
    /// The integrations asked for, served one at a time.
    queue: Queue,
}

impl Daemon {
    fn with_repo<T>(&self, f: impl FnOnce(&mut Repo) -> Result<T>) -> Result<T> {
        let mut guard = self.repo.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(repo) => f(repo),
            None => Err(Error::Other("the daemon is shutting down".into())),
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

    /// Time elapsed since the last watcher event (`None`: none has ever arrived).
    fn since_last_event(&self) -> Option<Duration> {
        if self.signals.events.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let last = Duration::from_millis(self.signals.last_event_ms.load(Ordering::Relaxed));
        Some(self.started.elapsed().saturating_sub(last))
    }

    /// Full reconciliation scan. If it finds changes while the watcher is
    /// silent, it means that not all events arrive here: from then on the
    /// daemon scans often instead of trusting the watcher.
    fn reconcile(&self) -> Result<usize> {
        let quiet = self.since_last_event().is_none_or(|t| t >= QUIET);
        let start = Instant::now();
        let ops = self.with_repo(|repo| repo.scan_all())?;
        self.last_scan_us.store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).rescans += 1;
        if !ops.is_empty() && quiet && self.watching {
            self.missed.fetch_add(ops.len() as u64, Ordering::Relaxed);
            if !self.fast.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "kitod: changes missed by the watcher: {}; from now on the directory is scanned often",
                    ops.len()
                );
                self.fast_changed.notify_one();
            }
        }
        Ok(ops.len())
    }

    /// Interval between two periodic scans.
    fn rescan_interval(&self) -> Duration {
        if !self.fast.load(Ordering::Relaxed) {
            return self.rescan_every;
        }
        let last_scan = Duration::from_micros(self.last_scan_us.load(Ordering::Relaxed));
        (last_scan * FAST_SCAN_FACTOR).max(FAST_SCAN_MIN).min(self.rescan_every)
    }

    /// Executes a request. It is blocking code: it must be called outside the runtime.
    fn handle(&self, request: &Request) -> Response {
        let id = request.id;
        let params = &request.params;
        let result: std::result::Result<Value, (i64, String)> = (|| {
            let internal = |e: Error| (rpc::INTERNAL_ERROR, e.to_string());
            let to_json = |v: std::result::Result<Value, serde_json::Error>| {
                v.map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))
            };
            let text = |key: &str| {
                params
                    .get(key)
                    .and_then(Value::as_str)
                    .ok_or((rpc::INVALID_PARAMS, format!("missing parameter `{key}`")))
            };
            let optional = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
            // The lane a request is about: the main one unless it says otherwise.
            let lane_or_main = || -> std::result::Result<LaneId, (i64, String)> {
                match params.get("lane").and_then(Value::as_str) {
                    Some(spec) => self.with_repo(|repo| repo.lane_of(spec)).map_err(internal),
                    None => Ok(LaneId::MAIN),
                }
            };
            let lane = || -> std::result::Result<LaneId, (i64, String)> {
                let spec = text("lane")?;
                self.with_repo(|repo| repo.lane_of(spec)).map_err(internal)
            };
            match request.method.as_str() {
                "status" => to_json(serde_json::to_value(self.status().map_err(internal)?)),
                "sync" => {
                    let ops = match params.get("lane").and_then(Value::as_str) {
                        // The barrier for one lane: what its agent saved is in the log.
                        Some(spec) => self
                            .with_repo(|repo| {
                                let lane = repo.lane_of(spec)?;
                                Ok(repo.scan_lane(lane)?.len())
                            })
                            .map_err(internal)?,
                        None => self.reconcile().map_err(internal)?,
                    };
                    Ok(json!({ "ops": ops }))
                }
                "log" => {
                    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                    let path = params.get("path").and_then(Value::as_str).map(PathBuf::from);
                    let lane = lane_or_main()?;
                    let items = self
                        .with_repo(|repo| repo.log_lane(lane, limit, path.as_deref()))
                        .map_err(internal)?;
                    to_json(serde_json::to_value(items))
                }
                "checkpoint" => {
                    let label = text("label")?;
                    let done = self.with_repo(|repo| repo.checkpoint(label)).map_err(internal)?;
                    to_json(serde_json::to_value(done))
                }
                "import_scip" => {
                    let file = PathBuf::from(text("file")?);
                    let done = self.with_repo(|repo| repo.import_scip(&file)).map_err(internal)?;
                    to_json(serde_json::to_value(done))
                }
                "export_git" => {
                    let done = self.with_repo(|repo| repo.export_git()).map_err(internal)?;
                    let list: Vec<Value> =
                        done.into_iter().map(|(c, g)| json!({ "checkpoint": c, "commit": g })).collect();
                    Ok(Value::Array(list))
                }
                "rollback" => {
                    let to = text("to")?;
                    let paths: Vec<PathBuf> = params
                        .get("paths")
                        .and_then(Value::as_array)
                        .map(|list| list.iter().filter_map(Value::as_str).map(PathBuf::from).collect())
                        .unwrap_or_default();
                    let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
                    let done = self.with_repo(|repo| repo.rollback(to, &paths, dry_run)).map_err(internal)?;
                    to_json(serde_json::to_value(done))
                }
                "reload" => {
                    self.with_repo(|repo| repo.reload_config()).map_err(internal)?;
                    let identity = self.with_repo(|repo| Ok(repo.identity().clone())).map_err(internal)?;
                    to_json(serde_json::to_value(identity))
                }
                "why" => {
                    let (file, symbol) = (PathBuf::from(text("file")?), text("symbol")?);
                    let lane = lane_or_main()?;
                    let found =
                        self.with_repo(|repo| repo.why_lane(lane, &file, symbol)).map_err(internal)?;
                    to_json(serde_json::to_value(found))
                }
                "lanes" => {
                    let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| Ok(repo.lanes(all))).map_err(internal)?,
                    ))
                }
                "open_lane" => {
                    let agent = text("agent")?;
                    let (task, base) = (optional("task"), optional("base"));
                    let intent = optional("intent").unwrap_or_default();
                    let scope = scope_of(params);
                    let (opened, overlaps) = self
                        .with_repo(|repo| {
                            let opened = repo.open_lane(agent, task, base.as_deref(), &intent, &scope)?;
                            // Who is already on the same code: told now, and
                            // not again as a notice.
                            let lane = repo.lane_of(&opened.id)?;
                            Ok((opened, repo.take_notices(lane)?))
                        })
                        .map_err(internal)?;
                    let mut opened =
                        serde_json::to_value(opened).map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
                    opened["overlaps"] = to_json(serde_json::to_value(overlaps))?;
                    Ok(opened)
                }
                "extend_scope" => {
                    let lane = lane()?;
                    let scope = scope_of(params);
                    let (declared, overlaps) = self
                        .with_repo(|repo| Ok((repo.extend_scope(lane, &scope)?, repo.take_notices(lane)?)))
                        .map_err(internal)?;
                    Ok(json!({
                        "lane": lane.to_string(),
                        "scope": to_json(serde_json::to_value(declared))?,
                        "overlaps": to_json(serde_json::to_value(overlaps))?,
                    }))
                }
                "board" => to_json(serde_json::to_value(
                    self.with_repo(|repo| {
                        // What is saved in each lane is part of what it is on.
                        for lane in repo.lanes(false) {
                            let lane = repo.lane_of(&lane.id)?;
                            repo.scan_lane(lane)?;
                        }
                        repo.board()
                    })
                    .map_err(internal)?,
                )),
                "who_touches" => {
                    let target = text("target")?;
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.who_touches(target)).map_err(internal)?,
                    ))
                }
                "notices" => {
                    let lane = lane()?;
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.take_notices(lane)).map_err(internal)?,
                    ))
                }
                "close_lane" => {
                    let lane = lane()?;
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.close_lane(lane)).map_err(internal)?,
                    ))
                }
                "lane_status" => {
                    let lane = lane()?;
                    let (info, conflicts) = self
                        .with_repo(|repo| {
                            let info = repo.lane_info(lane)?;
                            let mine: Vec<_> =
                                repo.conflicts(false).into_iter().filter(|c| c.lane == info.id).collect();
                            Ok((info, mine))
                        })
                        .map_err(internal)?;
                    let mut status =
                        serde_json::to_value(info).map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
                    status["conflicts"] = to_json(serde_json::to_value(conflicts))?;
                    Ok(status)
                }
                "set_intent" => {
                    let lane = lane_or_main()?;
                    let summary = text("summary")?;
                    let (task, prompt) = (optional("task"), optional("prompt"));
                    let intent = self
                        .with_repo(|repo| repo.set_intent(lane, summary, task, prompt.as_deref()))
                        .map_err(internal)?;
                    Ok(json!({ "intent": intent, "lane": lane.to_string() }))
                }
                "rename_symbol" => {
                    let lane = lane_or_main()?;
                    let (symbol, new_name) = (text("symbol")?, text("new_name")?);
                    // Who uses the symbol is asked without holding the
                    // repository: a language server takes its time.
                    let (plan, semantic) = self
                        .with_repo(|repo| Ok((repo.plan_rename(lane, symbol)?, repo.semantic())))
                        .map_err(internal)?;
                    let uses =
                        tokio::runtime::Handle::current().block_on(crate::refs::uses_of(&plan, &semantic));
                    let done = self
                        .with_repo(|repo| repo.rename_symbol_with(lane, symbol, new_name, Some(&uses)))
                        .map_err(internal)?;
                    to_json(serde_json::to_value(done))
                }
                "request_integration" => {
                    let lane = lane()?;
                    if lane.is_main() {
                        return Err((
                            rpc::INVALID_PARAMS,
                            "the main lane is what lanes are integrated into".into(),
                        ));
                    }
                    let job = self.queue.submit(lane);
                    Ok(json!({ "job": job.to_string(), "lane": lane.to_string() }))
                }
                "integration_result" => {
                    let job: JobId = text("job")?.parse().map_err(|e: String| (rpc::INVALID_PARAMS, e))?;
                    // Optionally waits for the result, up to a limit.
                    let wait =
                        Duration::from_millis(params.get("wait_ms").and_then(Value::as_u64).unwrap_or(0));
                    let asked = Instant::now();
                    loop {
                        let state = self
                            .queue
                            .state(job)
                            .ok_or((rpc::INVALID_PARAMS, format!("{job}: no such integration request")))?;
                        if matches!(state, JobState::Done { .. }) || asked.elapsed() >= wait {
                            return to_json(serde_json::to_value(state));
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                "conflicts" => {
                    let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
                    let list = self.with_repo(|repo| Ok(repo.conflicts(all))).map_err(internal)?;
                    to_json(serde_json::to_value(list))
                }
                "resolve_conflict" => {
                    let id: ConflictId = text("conflict")?.parse().map_err(|_| {
                        (rpc::INVALID_PARAMS, "`conflict` must be like conflict#7".to_owned())
                    })?;
                    let decision = match (params.get("choice").and_then(Value::as_str), optional("text")) {
                        (_, Some(text)) => Decision::Text(text),
                        (Some("main"), None) => Decision::Main,
                        (Some("lane"), None) => Decision::Lane,
                        (Some("rewrite"), None) => Decision::Rewrite,
                        (Some("keep"), None) => Decision::Keep,
                        _ => {
                            return Err((
                                rpc::INVALID_PARAMS,
                                "give `choice` (\"main\" or \"lane\"; for the uses of a renamed symbol, \"rewrite\" or \"keep\") or the new `text`".to_owned(),
                            ))
                        }
                    };
                    let (conflict, remaining) = self
                        .with_repo(|repo| {
                            let conflict = repo.resolve_conflict(id, decision)?;
                            let remaining = repo
                                .conflicts(false)
                                .iter()
                                .filter(|c| c.lane == conflict.lane && c.resolution.is_none())
                                .count();
                            Ok((conflict, remaining))
                        })
                        .map_err(internal)?;
                    Ok(json!({
                        "conflict": to_json(serde_json::to_value(conflict))?,
                        "remaining": remaining,
                        "next": if remaining == 0 { "ask for the integration again: the decisions are applied by it" } else { "decide the remaining conflicts" },
                    }))
                }
                "shutdown" => {
                    self.shutdown.notify_one();
                    Ok(json!({}))
                }
                other => Err((rpc::METHOD_NOT_FOUND, format!("unknown method: {other}"))),
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

/// `true` for the paths that the daemon itself writes or that are never
/// tracked: it is not even worth waking up the capture.
fn is_internal(rel: &Path) -> bool {
    let mut parts = rel.components();
    match parts.next() {
        Some(Component::Normal(first)) if first == ".kito" => true,
        Some(Component::Normal(first)) if first == ".git" => true,
        _ => rel.components().any(|c| c.as_os_str() == ".git"),
    }
}

/// See [`WatchMsg::Paths`].
type Whole = Option<Option<crate::repo::FileStat>>;

/// How long to wait for more events once every file of a batch is known to
/// be whole: long enough for the rename that follows a save to a temporary
/// file, not the whole debounce.
const QUICK: Duration = Duration::from_millis(2);

enum WatchMsg {
    /// Paths that changed, each relative to the working directory of its
    /// lane. With `Some` for an event that says the file is whole now
    /// (closed after being written, put in place by a rename, removed):
    /// what the file system said of it at that moment, `None` if gone.
    Paths(Vec<(LaneId, PathBuf, Whole)>),
    Rescan(String),
}

/// Which working directory a path of the repository belongs to: the main
/// one, or a lane's (`.kito/lanes/<id>/work/…`). `None` for what is never
/// captured.
fn route(rel: &Path) -> Option<(LaneId, PathBuf)> {
    if !is_internal(rel) {
        return Some((LaneId::MAIN, rel.to_owned()));
    }
    let mut parts = rel.components();
    let (kito, lanes, id, work) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if kito.as_os_str() != ".kito" || lanes.as_os_str() != "lanes" || work.as_os_str() != "work" {
        return None;
    }
    let lane = LaneId(u64::from_str_radix(id.as_os_str().to_str()?, 16).ok()?);
    let inside = parts.as_path();
    (!inside.as_os_str().is_empty() && !is_internal(inside)).then(|| (lane, inside.to_owned()))
}

/// Collects the watcher events and hands them to the capture after a period
/// of quiet (debounce).
async fn debounce_loop(daemon: Arc<Daemon>, mut events: mpsc::UnboundedReceiver<WatchMsg>) {
    let max_wait = daemon.debounce * MAX_WAIT_FACTOR;
    while let Some(first) = events.recv().await {
        let batch_start = Instant::now();
        // Per path, what its last event said.
        let mut paths: BTreeMap<LaneId, BTreeMap<PathBuf, Whole>> = BTreeMap::new();
        let mut rescan: Option<String> = None;
        let mut absorb = |msg: WatchMsg, paths: &mut BTreeMap<LaneId, BTreeMap<PathBuf, Whole>>| match msg {
            WatchMsg::Paths(list) => {
                for (lane, path, whole) in list {
                    paths.entry(lane).or_default().insert(path, whole);
                }
            }
            WatchMsg::Rescan(why) => rescan = Some(why),
        };
        absorb(first, &mut paths);
        loop {
            let left = max_wait.saturating_sub(batch_start.elapsed());
            if left.is_zero() {
                break;
            }
            // Quiet is waited for because a save may not be over. When the
            // system has said of every file that it is whole, it is over.
            let all_whole = paths.values().all(|lane| lane.values().all(Option::is_some));
            let quiet = if all_whole { QUICK } else { daemon.debounce };
            match tokio::time::timeout(quiet.min(left), events.recv()).await {
                Ok(Some(msg)) => absorb(msg, &mut paths),
                Ok(None) | Err(_) => break,
            }
        }

        let worker = daemon.clone();
        let capture_start = Instant::now();
        let needs_rescan = rescan.is_some();
        let outcome = tokio::task::spawn_blocking(move || {
            worker.with_repo(|repo| {
                if needs_rescan {
                    return repo.scan_all();
                }
                let mut ops = Vec::new();
                for (lane, paths) in paths {
                    match repo.capture_reported(lane, paths) {
                        Ok(captured) => ops.extend(captured),
                        // Events of a lane that was closed in the meantime.
                        Err(Error::NoSuchLane(_)) => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok(ops)
            })
        })
        .await;
        let (mut captured, mut error) = match outcome {
            Ok(Ok(ops)) => (!ops.is_empty(), rescan),
            Ok(Err(e)) => (false, Some(e.to_string())),
            Err(e) => (false, Some(e.to_string())),
        };
        // Files that were being written at that very moment: once they
        // have settled they are looked at again.
        for _ in 0..200 {
            if !daemon.with_repo(|repo| Ok(repo.has_deferred())).unwrap_or(false) {
                break;
            }
            tokio::time::sleep(crate::repo::SETTLE).await;
            let worker = daemon.clone();
            match tokio::task::spawn_blocking(move || worker.with_repo(|repo| repo.capture_deferred())).await
            {
                Ok(Ok(ops)) => captured |= !ops.is_empty(),
                Ok(Err(e)) => error = Some(e.to_string()),
                Err(e) => error = Some(e.to_string()),
            }
        }
        {
            let mut stats = daemon.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.batches += 1;
            stats.rescans += u64::from(needs_rescan);
        }
        if let Some(message) = &error {
            eprintln!("kitod: capture: {message}");
        }
        // Latency is measured only on the batches that produced ops.
        daemon.record(capture_start.elapsed(), captured.then(|| batch_start.elapsed()), error);
    }
}

/// Periodic scan: recovers the events the watcher missed.
async fn rescan_loop(daemon: Arc<Daemon>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(daemon.rescan_interval()) => {}
            // Frequent scanning has just kicked in: the wait is recomputed.
            _ = daemon.fast_changed.notified() => continue,
        }
        let worker = daemon.clone();
        match tokio::task::spawn_blocking(move || worker.reconcile()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("kitod: periodic scan: {e}"),
            Err(_) => return,
        }
    }
}

/// One integration, in its three steps. The repository is held only while
/// merging and while recording: the verification runs with it free.
async fn integrate(daemon: &Arc<Daemon>, lane: LaneId) -> IntegrationResult {
    let failed = |message: String| IntegrationResult::failed(lane.to_string(), message);
    let mut answers: Option<crate::refs::RefAnswers> = None;
    for _ in 0..6 {
        let worker = daemon.clone();
        let given = answers.take();
        let prepared = tokio::task::spawn_blocking(move || {
            worker.with_repo(|repo| match repo.prepare_integration(lane, given.as_ref())? {
                Preparation::Done(result) => Ok(Ok(Err(result))),
                Preparation::Ready(prepared) => {
                    let copy = repo.verification_copy(&prepared)?;
                    Ok(Ok(Ok((prepared, copy))))
                }
                Preparation::Ask(plan) => Ok(Err((plan, repo.semantic()))),
            })
        })
        .await;
        let (prepared, copy) = match prepared {
            Ok(Ok(Ok(Ok(ready)))) => ready,
            Ok(Ok(Ok(Err(result)))) => return *result,
            // Level 2: who uses what one of the two sides renamed. Asked
            // without holding the repository: a language server takes its
            // time. Then the merge is prepared again, with the answers.
            Ok(Ok(Err((plan, semantic)))) => {
                answers = Some(crate::refs::answer(&plan, &semantic).await);
                continue;
            }
            Ok(Err(e)) => return failed(e.to_string()),
            Err(e) => return failed(e.to_string()),
        };
        let report = match copy {
            Some((dir, commands, timeout)) => Some(kito_integrate::verify(&dir, &commands, timeout).await),
            None => None,
        };
        let worker = daemon.clone();
        let finished = tokio::task::spawn_blocking(move || {
            worker.with_repo(|repo| repo.finish_integration(*prepared, report))
        })
        .await;
        match finished {
            Ok(Ok(Some(result))) => return result,
            // A lane moved while the verification ran: merge again.
            Ok(Ok(None)) => continue,
            Ok(Err(e)) => return failed(e.to_string()),
            Err(e) => return failed(e.to_string()),
        }
    }
    failed("the lanes keep changing: ask again when they are quiet".to_owned())
}

/// Serves the integration requests, one at a time, in order.
async fn integration_loop(daemon: Arc<Daemon>) {
    loop {
        let (job, lane) = daemon.queue.next().await;
        let result = integrate(&daemon, lane).await;
        daemon.queue.finish(job, result);
        // The lane has its answer: now git's index, left behind by the
        // export of the integration, is brought up to the commit.
        let worker = daemon.clone();
        let refreshed = tokio::task::spawn_blocking(move || {
            match worker.with_repo(|repo| Ok(repo.take_index_refresh())) {
                Ok(Some((root, options))) => {
                    kito_git::refresh_index(&root, &options).map_err(|e| e.to_string())
                }
                Ok(None) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        })
        .await;
        if let Ok(Err(e)) = refreshed {
            eprintln!("kitod: git index: {e}");
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
            Err(e) => Response::err(0, rpc::PARSE_ERROR, format!("invalid request: {e}")),
        };
        let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
        bytes.push(b'\n');
        if writer.write_all(&bytes).await.is_err() {
            break;
        }
    }
}

/// Runs the daemon of the repository in `root` until shutdown (`shutdown`
/// request, SIGTERM or Ctrl-C).
pub async fn serve(root: &Path) -> Result<()> {
    // Opening the store takes the exclusive lock: only one daemon per repository.
    let mut repo = Repo::open(root)?;
    let root = repo.root().to_owned();
    let debounce = Duration::from_millis(repo.config().capture.debounce_ms.max(1));
    let rescan_every = Duration::from_secs(repo.config().capture.rescan_secs.max(1));

    let watching = repo.config().capture.watch;
    let filesystem = crate::fsinfo::filesystem_type(&root);
    let unreliable = filesystem.as_deref().is_some_and(crate::fsinfo::events_are_unreliable);
    let started = Instant::now();

    // The watcher starts before the initial scan: this way no change can
    // fall into the gap between the two.
    let (tx, rx) = mpsc::unbounded_channel::<WatchMsg>();
    let signals = Arc::new(Signals::default());
    let watcher = if watching {
        let watch_root = root.clone();
        let seen = signals.clone();
        let writing = repo.writing();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            // Which files are in the middle of a save: written to, and not
            // closed yet (the events say when, where the system tells).
            if let Ok(event) = &event {
                use notify::event::{AccessKind, AccessMode, ModifyKind};
                match event.kind {
                    notify::EventKind::Modify(ModifyKind::Data(_)) if CLOSE_IS_REPORTED => {
                        event.paths.iter().for_each(|path| writing.modified(path));
                    }
                    notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => {
                        event.paths.iter().for_each(|path| writing.closed(path));
                    }
                    _ => {}
                }
            }
            let closed_after_writing = |kind: &notify::EventKind| {
                use notify::event::{AccessKind, AccessMode};
                matches!(kind, notify::EventKind::Access(AccessKind::Close(AccessMode::Write)))
            };
            let msg = match event {
                Ok(event) if event.need_rescan() => WatchMsg::Rescan("the watcher missed some events".into()),
                // Reads and opens say nothing; a file closed after being
                // written is the moment its save is complete.
                Ok(event)
                    if matches!(event.kind, notify::EventKind::Access(_))
                        && !closed_after_writing(&event.kind) =>
                {
                    return
                }
                Ok(event) => {
                    // Events after which nothing more is to come for the
                    // file: what the file system says of it right now is
                    // what a capture must find to take it at once.
                    use notify::event::ModifyKind;
                    let whole = closed_after_writing(&event.kind)
                        || matches!(
                            event.kind,
                            notify::EventKind::Remove(_)
                                | notify::EventKind::Modify(ModifyKind::Name(_) | ModifyKind::Metadata(_))
                        );
                    let paths: Vec<(LaneId, PathBuf, Whole)> = event
                        .paths
                        .iter()
                        .filter_map(|abs| {
                            let rel = abs.strip_prefix(&watch_root).ok()?;
                            let (lane, path) = route(rel).filter(|_| !rel.as_os_str().is_empty())?;
                            let now = || {
                                let meta = std::fs::symlink_metadata(abs).ok()?;
                                meta.is_file().then(|| crate::repo::FileStat::of(&meta))
                            };
                            Some((lane, path, (whole && CLOSE_IS_REPORTED).then(now)))
                        })
                        .collect();
                    if paths.is_empty() {
                        return;
                    }
                    seen.last_event_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    seen.events.fetch_add(1, Ordering::Relaxed);
                    WatchMsg::Paths(paths)
                }
                Err(e) => WatchMsg::Rescan(format!("watcher error: {e}")),
            };
            let _ = tx.send(msg);
        })
        .map_err(|e| Error::Other(format!("starting the watcher: {e}")))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| Error::Other(format!("watcher on {}: {e}", root.display())))?;
        Some(watcher)
    } else {
        drop(tx);
        None
    };

    repo.scan_all()?;
    // Files known only as text (imported, or captured by an older version) get their symbols now.
    repo.analyze()?;

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
        queue: Queue::new(),
    });
    eprintln!("kitod: listening on {} for {}", socket.display(), daemon.root.display());
    if !watching {
        eprintln!("kitod: watcher disabled by the configuration: the directory is scanned often");
    } else if unreliable {
        eprintln!(
            "kitod: file system {}: file events are unreliable, the directory is scanned often",
            daemon.filesystem.as_deref().unwrap_or("?")
        );
    }

    let debouncer = tokio::spawn(debounce_loop(daemon.clone(), rx));
    let rescanner = tokio::spawn(rescan_loop(daemon.clone()));
    let integrator = tokio::spawn(integration_loop(daemon.clone()));
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| Error::Other(format!("signal handling: {e}")))?;

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

    // From here on no new connection is accepted. The socket file stays
    // until the store is closed: whoever is waiting for the shutdown
    // (`kito daemon stop`) knows that once it disappears the lock is free.
    drop(listener);
    drop(watcher);
    debouncer.abort();
    rescanner.abort();
    integrator.abort();
    // Last scan and clean shutdown: state saved, indexes durable.
    let repo = daemon.repo.lock().unwrap_or_else(|e| e.into_inner()).take();
    let closed = match repo {
        Some(mut repo) => tokio::task::spawn_blocking(move || {
            repo.scan_all()?;
            repo.close()
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))
        .and_then(|r| r),
        None => Ok(()),
    };
    let _ = std::fs::remove_file(&socket);
    closed?;
    eprintln!("kitod: stopped");
    Ok(())
}

/// The scope a request carries: a list of items, or one.
fn scope_of(params: &Value) -> Vec<String> {
    match params.get("scope") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        Some(Value::String(item)) => vec![item.clone()],
        _ => Vec::new(),
    }
}
