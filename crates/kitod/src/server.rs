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
    /// What happened lately, for the clients that watch the repository.
    events: Events,
    /// Which copy on another machine writes in each lane: one at a time.
    writers: Mutex<std::collections::BTreeMap<LaneId, String>>,
}

/// How many changed files a `saved` event lists: above, it lists none and
/// whoever keeps a copy looks at everything.
const PATHS_TOLD: usize = 500;

/// How much content one `get_blobs` answer carries at most.
const BLOBS_AT_ONCE: usize = 8 << 20;

/// How many events are kept for a client that asks late.
const EVENTS_KEPT: usize = 4096;

/// What happened in the repository, numbered, with a way to wait for more.
#[derive(Default)]
struct Events {
    /// The number of the last event, and the last ones.
    log: Mutex<(u64, std::collections::VecDeque<rpc::Event>)>,
    news: std::sync::Condvar,
}

impl Events {
    fn emit(&self, kind: &str, lane: Option<String>, by: Option<String>, what: String) {
        self.emit_with(kind, lane, by, what, Vec::new());
    }

    fn emit_with(
        &self,
        kind: &str,
        lane: Option<String>,
        by: Option<String>,
        what: String,
        paths: Vec<String>,
    ) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.0 += 1;
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        let event = rpc::Event { seq: log.0, at_ms, kind: kind.to_owned(), lane, by, what, paths };
        log.1.push_back(event);
        while log.1.len() > EVENTS_KEPT {
            log.1.pop_front();
        }
        self.news.notify_all();
    }

    /// The events after number `after`, waiting up to `wait` for the first
    /// one; with no `after`, only where the numbering is now.
    fn after(&self, after: Option<u64>, wait: Duration) -> (Vec<rpc::Event>, u64) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let Some(after) = after else { return (Vec::new(), log.0) };
        let deadline = Instant::now() + wait;
        while log.0 <= after {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            log = self.news.wait_timeout(log, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        (log.1.iter().filter(|event| event.seq > after).cloned().collect(), log.0)
    }
}

impl Daemon {
    /// A lane has one copy elsewhere that writes in it: the first to
    /// write, until another claims the lane (`claim_writer`).
    fn writes(&self, lane: LaneId, writer: Option<&str>) -> Result<()> {
        let Some(writer) = writer else { return Ok(()) };
        let mut writers = self.writers.lock().unwrap_or_else(|e| e.into_inner());
        match writers.get(&lane) {
            Some(current) if current != writer => Err(Error::NotTheWriter(format!(
                "{lane} is written from {} now: `kito lane attach {lane} --write` takes it back",
                current.split(':').next().unwrap_or(current)
            ))),
            Some(_) => Ok(()),
            None => {
                writers.insert(lane, writer.to_owned());
                Ok(())
            }
        }
    }

    fn with_repo<T>(&self, f: impl FnOnce(&mut Repo) -> Result<T>) -> Result<T> {
        let mut guard = self.repo.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(repo) => {
                let result = f(repo);
                // Whatever was recorded on the way, the watchers are told.
                let recorded = std::mem::take(&mut repo.recorded);
                drop(guard);
                self.saved(&recorded);
                result
            }
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
            let internal = |e: Error| match e {
                Error::Refused(why) => (rpc::REFUSED, why),
                Error::NotTheWriter(why) => (rpc::NOT_THE_WRITER, why),
                e @ Error::NoSuchLane(_) => (rpc::NO_SUCH_LANE, e.to_string()),
                e => (rpc::INTERNAL_ERROR, e.to_string()),
            };
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
                // Asked by whoever wants to know that the daemon is there.
                "ping" => Ok(json!({ "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id() })),
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
                            let lane = repo.lane_of(&opened.id)?;
                            // The person a server authenticated: the lane is theirs.
                            let opened = match params.get("on_behalf_of").and_then(person_of) {
                                Some(person) => {
                                    repo.set_lane_owner(lane, person)?;
                                    repo.lane_info(lane)?
                                }
                                None => opened,
                            };
                            // Who is already on the same code: told now, and
                            // not again as a notice.
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
                "lane_files" => {
                    let lane = lane_or_main()?;
                    let paths: Option<Vec<PathBuf>> = params
                        .get("paths")
                        .and_then(Value::as_array)
                        .map(|paths| paths.iter().filter_map(Value::as_str).map(PathBuf::from).collect());
                    let (files, ops) = self
                        .with_repo(|repo| {
                            Ok((repo.lane_files(lane, paths.as_deref())?, repo.lane_ops(lane)?))
                        })
                        .map_err(internal)?;
                    Ok(
                        json!({ "lane": lane.to_string(), "ops": ops, "files": to_json(serde_json::to_value(files))? }),
                    )
                }
                "get_blobs" => {
                    let ids: Vec<&str> = params
                        .get("blobs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    // As many as fit in one answer: the client asks again for the rest.
                    let mut blobs = serde_json::Map::new();
                    let mut size = 0;
                    for id in ids {
                        let content = self.with_repo(|repo| repo.content(id)).map_err(internal)?;
                        size += content.len();
                        blobs.insert(id.to_owned(), json!(rpc::encode(&content)));
                        if size >= BLOBS_AT_ONCE {
                            break;
                        }
                    }
                    Ok(json!({ "blobs": blobs }))
                }
                "put_file" => {
                    let lane = lane()?;
                    let path = text("path")?;
                    let content = rpc::decode(text("content")?)
                        .ok_or((rpc::INVALID_PARAMS, "`content` is not base64".to_owned()))?;
                    let executable = params.get("executable").and_then(Value::as_bool).unwrap_or(false);
                    let (base, person) = (optional("base"), person_name(params));
                    let put = self
                        .with_repo(|repo| {
                            repo.may_write(lane, person.as_deref())?;
                            self.writes(lane, optional("writer").as_deref())?;
                            repo.put_file(lane, path, &content, executable, base.as_deref())
                        })
                        .map_err(internal)?;
                    to_json(serde_json::to_value(put))
                }
                "remove_file" => {
                    let lane = lane()?;
                    let path = text("path")?;
                    let (base, person) = (optional("base"), person_name(params));
                    let put = self
                        .with_repo(|repo| {
                            repo.may_write(lane, person.as_deref())?;
                            self.writes(lane, optional("writer").as_deref())?;
                            repo.remove_file(lane, path, base.as_deref())
                        })
                        .map_err(internal)?;
                    to_json(serde_json::to_value(put))
                }
                "claim_writer" => {
                    let lane = lane()?;
                    let writer = text("writer")?.to_owned();
                    let person = person_name(params);
                    self.with_repo(|repo| {
                        if lane.is_main() {
                            return Err(Error::Refused("the main lane has no writer: open a lane".into()));
                        }
                        repo.may_write(lane, person.as_deref())
                    })
                    .map_err(internal)?;
                    let before =
                        self.writers.lock().unwrap_or_else(|e| e.into_inner()).insert(lane, writer.clone());
                    Ok(json!({ "lane": lane.to_string(), "writer": writer, "before": before }))
                }
                "events" => {
                    let after = params.get("after").and_then(Value::as_u64);
                    let wait = params.get("wait_ms").and_then(Value::as_u64).unwrap_or(25_000).min(60_000);
                    let (events, next) = self.events.after(after, Duration::from_millis(wait));
                    Ok(json!({ "events": to_json(serde_json::to_value(events))?, "next": next }))
                }
                "notices" => {
                    let lane = lane()?;
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.take_notices(lane)).map_err(internal)?,
                    ))
                }
                "close_lane" => {
                    let lane = lane()?;
                    self.writers.lock().unwrap_or_else(|e| e.into_inner()).remove(&lane);
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
            Ok(value) => {
                self.note(&request.method, params, &value);
                Response::ok(id, value)
            }
            Err((code, message)) => Response::err(id, code, message),
        }
    }

    /// Tells the clients that watch the repository what a request did.
    fn note(&self, method: &str, params: &Value, value: &Value) {
        let text = |from: &Value, key: &str| from.get(key).and_then(Value::as_str).map(str::to_owned);
        let person = params.get("on_behalf_of").and_then(|person| text(person, "name"));
        let lane = text(params, "lane").or_else(|| text(value, "lane"));
        let list = |from: &Value, key: &str| -> String {
            let items: Vec<&str> = from
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            items.join(", ")
        };
        let (kind, lane, by, what) = match method {
            "open_lane" => {
                let agent = text(value, "agent").unwrap_or_default();
                let what = format!(
                    "{agent}: {} [{}]",
                    text(value, "intent").unwrap_or_default(),
                    list(value, "scope")
                );
                ("lane_opened", text(value, "id"), person.or(Some(agent)), what)
            }
            "close_lane" => ("lane_closed", text(value, "id"), person, String::new()),
            "extend_scope" => ("scope_extended", lane, person, list(params, "scope")),
            "set_intent" => ("intent", lane, person, text(params, "summary").unwrap_or_default()),
            "rename_symbol" => {
                let what = format!(
                    "{} → {}",
                    text(value, "from").unwrap_or_default(),
                    text(value, "to").unwrap_or_default()
                );
                ("renamed", lane, person, what)
            }
            "request_integration" => ("integration_requested", lane, person, String::new()),
            "resolve_conflict" => {
                ("conflict_resolved", None, person, text(params, "conflict").unwrap_or_default())
            }
            "checkpoint" => {
                ("checkpoint", Some("main".to_owned()), person, text(value, "label").unwrap_or_default())
            }
            _ => return,
        };
        self.events.emit(kind, lane, by, what);
    }

    /// Tells the clients what was just recorded: one event for each lane.
    fn saved(&self, ops: &[kito_core::Op]) {
        let mut by_lane: std::collections::BTreeMap<LaneId, (String, std::collections::BTreeSet<String>)> =
            Default::default();
        for op in ops {
            let entry =
                by_lane.entry(op.lane).or_insert_with(|| (op.author.name.clone(), Default::default()));
            if op.kind.symbol().is_none() {
                entry.1.extend(op.kind.paths().iter().map(|path| path.display().to_string()));
            }
        }
        for (lane, (author, files)) in by_lane {
            if files.is_empty() {
                continue;
            }
            let shown: Vec<&str> = files.iter().take(3).map(String::as_str).collect();
            let more = match files.len() - shown.len() {
                0 => String::new(),
                n => format!(" and {n} more"),
            };
            let what = format!("{}{more}", shown.join(", "));
            // The paths themselves, for whoever keeps a copy of the lane.
            let paths: Vec<String> = match files.len() <= PATHS_TOLD {
                true => files.into_iter().collect(),
                false => Vec::new(),
            };
            self.events.emit_with("saved", Some(lane.to_string()), Some(author), what, paths);
        }
    }

    /// Tells the clients how an integration ended.
    fn integration_ended(&self, result: &kito_integrate::IntegrationResult) {
        let kind = serde_json::to_value(result.result)
            .ok()
            .and_then(|kind| kind.as_str().map(str::to_owned))
            .unwrap_or_else(|| "failed".to_owned());
        let what = match result.result {
            kito_integrate::Outcome::Integrated => result.checkpoint.clone().unwrap_or_default(),
            kito_integrate::Outcome::Conflicts => format!("{} to decide", result.conflicts.len()),
            kito_integrate::Outcome::OutOfScope => result.out_of_scope.join(", "),
            kito_integrate::Outcome::VerificationFailed => result
                .verification
                .as_ref()
                .and_then(|report| report.failed_command.clone())
                .unwrap_or_default(),
            _ => result.message.clone().unwrap_or_default(),
        };
        self.events.emit(&kind, Some(result.lane.clone()), None, what);
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
        daemon.integration_ended(&result);
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
        events: Events::default(),
        writers: Mutex::default(),
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

/// The person a request is made for, as a server that authenticated them
/// says: `{ "name": ..., "email": ... }`.
fn person_of(value: &Value) -> Option<kito_core::Author> {
    let name = value.get("name").and_then(Value::as_str).filter(|name| !name.trim().is_empty())?;
    let person = kito_core::Author::human(name.trim());
    Some(match value.get("email").and_then(Value::as_str).filter(|email| !email.is_empty()) {
        Some(email) => person.with_email(email),
        None => person,
    })
}

/// The name of the person a request is made for, when a server said so.
fn person_name(params: &Value) -> Option<String> {
    params.get("on_behalf_of").and_then(person_of).map(|person| person.name)
}
