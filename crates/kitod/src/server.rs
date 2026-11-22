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
use tokio::sync::{mpsc, Notify};

/// What stops the daemon besides a `shutdown` request and Ctrl-C: SIGTERM,
/// or Ctrl-Break on Windows (ADR 0002).
#[cfg(unix)]
fn terminate_signal() -> Result<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| Error::Other(format!("signal handling: {e}")))
}

#[cfg(windows)]
fn terminate_signal() -> Result<tokio::signal::windows::CtrlBreak> {
    tokio::signal::windows::ctrl_break().map_err(|e| Error::Other(format!("signal handling: {e}")))
}

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
    /// What each plugin last failed at, if it has not run well since.
    plugin_failures: Mutex<std::collections::BTreeMap<String, String>>,
    /// Counts the times a plugin was enabled or disabled: who listens to
    /// events is read again then.
    plugins_changed: AtomicU64,
    /// What the sessions of assistants said they run on, the last one
    /// said last (ADR 0010). Kept while the daemon runs, and no longer.
    sessions: Mutex<Vec<(String, kito_core::AssistantSaid)>>,
    root: PathBuf,
    socket: PathBuf,
    /// What a client says first on the socket. `None` if it could not be
    /// made: the socket then answers anybody, as it used to.
    key: Option<String>,
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
    /// Who asked for each integration, when a server said so, and whether
    /// they may integrate. For one who may not, the merge and the checks
    /// are done and the lane then waits.
    holds: Mutex<std::collections::BTreeMap<JobId, (kito_core::Author, bool)>>,
    /// Told when a new checkpoint may have to go to the git remote.
    push_wanted: Notify,
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

    /// Who a first line is from: the key of the socket is whoever can
    /// read the repository's own data, the key of a plugin is that plugin.
    fn who_said(&self, line: &str) -> Option<crate::plugins::Access> {
        let key = self.key.as_deref()?;
        if rpc::key_said(line, key)? {
            return Some(crate::plugins::Access::Owner);
        }
        let said: Value = serde_json::from_str(line).ok()?;
        let said = said.get("key")?.as_str()?.to_owned();
        self.with_repo(|repo| Ok(repo.plugin_of_key(&said))).ok().flatten()
    }

    fn with_repo<T>(&self, f: impl FnOnce(&mut Repo) -> Result<T>) -> Result<T> {
        let mut guard = self.repo.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(repo) => {
                let result = f(repo);
                // Whatever was recorded on the way, the watchers are told.
                let recorded = std::mem::take(&mut repo.recorded);
                let refused = std::mem::take(&mut repo.refused);
                drop(guard);
                self.saved(&recorded);
                for refusal in refused {
                    let kept = match &refusal.kept {
                        Some(dir) => format!("; the edit is kept in {}", dir.display()),
                        None => String::new(),
                    };
                    let what = format!("{} put back as the main lane has it{kept}", refusal.paths.join(", "));
                    self.events.emit_with(
                        "main_edit_refused",
                        Some(LaneId::MAIN.to_string()),
                        None,
                        what,
                        refusal.paths,
                    );
                }
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
    /// How many sessions are remembered: one that started long ago says
    /// what it runs on again when it starts again.
    const SESSIONS_KEPT: usize = 256;

    /// Remembers what a session of an assistant said it runs on (ADR 0010).
    fn session_said(&self, session: &str, said: kito_core::AssistantSaid) {
        let said = said.clean();
        if session.is_empty() || session.len() > 128 || said.is_empty() {
            return;
        }
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let known = match sessions.iter().position(|(id, _)| id == session) {
            Some(at) => sessions.remove(at).1,
            None => kito_core::AssistantSaid::default(),
        };
        sessions.push((session.to_owned(), known.with(said)));
        if sessions.len() > Self::SESSIONS_KEPT {
            sessions.remove(0);
        }
    }

    /// What a request says of the assistant behind it: what its session
    /// said when it started, completed by what the request itself says.
    fn said_of(&self, params: &Value) -> kito_core::AssistantSaid {
        let own: kito_core::AssistantSaid = params
            .get("assistant")
            .and_then(|said| serde_json::from_value(said.clone()).ok())
            .unwrap_or_default();
        let own = own.clean();
        let Some(session) = params.get("session").and_then(Value::as_str) else { return own };
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        match sessions.iter().find(|(id, _)| id == session) {
            Some((_, known)) => known.clone().with(own),
            None => own,
        }
    }

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
            // A lane is changed by its owner, or by somebody who may
            // integrate: asked of requests a server passed on for a person.
            let owned = |lane: LaneId| -> std::result::Result<(), (i64, String)> {
                if lane.is_main() {
                    return Ok(());
                }
                self.with_repo(|repo| {
                    repo.may_change(lane, person_name(params).as_deref(), integrates(params))
                })
                .map_err(internal)
            };
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
                "configure" => {
                    let checks: Option<Vec<String>> =
                        params.get("checks").and_then(|checks| serde_json::from_value(checks.clone()).ok());
                    let push = params.get("push").map(|remote| remote.as_str().map(str::to_owned));
                    let approvals = params.get("approvals").and_then(Value::as_u64);
                    let kitchen = params.get("kitchen").map(|kitchen| kitchen.as_str().map(str::to_owned));
                    let (now, approvals, kitchen_now) = self
                        .with_repo(|repo| {
                            repo.configure(checks, push)?;
                            if let Some(approvals) = approvals {
                                repo.set_required_approvals(approvals.min(99) as u32)?;
                            }
                            if let Some(kitchen) = kitchen.clone() {
                                repo.set_kitchen(kitchen)?;
                            }
                            Ok((
                                repo.status()?,
                                repo.required_approvals(),
                                repo.config().kitchen.path.clone(),
                            ))
                        })
                        .map_err(internal)?;
                    // A remote just named gets what is already there.
                    self.push_wanted.notify_one();
                    if kitchen.is_some() {
                        let what = match &kitchen_now {
                            Some(path) => format!("the kitchen is {path}"),
                            None => "the standard kitchen".to_owned(),
                        };
                        self.events.emit("instructions", None, person_name(params), what);
                    }
                    Ok(json!({
                        "protected": now.protected,
                        "checks": now.checks,
                        "push": now.push,
                        "approvals": approvals,
                        "kitchen": kitchen_now,
                    }))
                }
                // Tasks (ADR 0015): kept by Kito, out of the project's files.
                "tasks" | "task" => {
                    let (root, lanes) = self
                        .with_repo(|repo| Ok((repo.root().to_owned(), repo.lanes(true))))
                        .map_err(internal)?;
                    let all = crate::tasks::load(&root);
                    if request.method == "task" {
                        let n = crate::tasks::number(text("task")?)
                            .ok_or((rpc::INVALID_PARAMS, "a task is task#<n>".to_owned()))?;
                        let task =
                            all.get(n).ok_or((rpc::INVALID_PARAMS, format!("task#{n}: no such task")))?;
                        return to_json(serde_json::to_value(crate::tasks::info(task, &lanes, true)));
                    }
                    let state = optional("state").unwrap_or_else(|| "open".to_owned());
                    let assignee = optional("for");
                    let listed: Vec<crate::tasks::TaskInfo> = all
                        .tasks
                        .iter()
                        .map(|task| crate::tasks::info(task, &lanes, false))
                        .filter(|task| match state.as_str() {
                            "all" => true,
                            "open" => !matches!(task.state, "done" | "dropped"),
                            wanted => task.state == wanted,
                        })
                        .filter(|task| assignee.is_none() || task.assignee == assignee)
                        .collect();
                    to_json(serde_json::to_value(listed))
                }
                "task_add" | "task_update" | "task_claim" | "task_comment" | "task_done" | "task_reopen"
                | "task_drop" => {
                    let by = person_name(params)
                        .or_else(|| self.with_repo(|repo| Ok(repo.identity.name.clone())).ok())
                        .filter(|by| !by.is_empty())
                        .unwrap_or_else(|| "this machine".to_owned());
                    let agent = optional("agent").filter(|agent| !agent.trim().is_empty());
                    let source = params.get("source").filter(|source| !source.is_null()).cloned();
                    let (root, lanes) = self
                        .with_repo(|repo| Ok((repo.root().to_owned(), repo.lanes(true))))
                        .map_err(internal)?;
                    let refused = |why: String| (rpc::REFUSED, why);
                    let mut all = crate::tasks::load(&root);
                    let now = crate::tasks::now_ms();
                    let method = request.method.as_str();
                    let (n, what) = if method == "task_add" {
                        let n = all
                            .add(
                                text("title")?,
                                &optional("text").unwrap_or_default(),
                                &by,
                                optional("for"),
                                source.clone(),
                            )
                            .map_err(refused)?;
                        (n, format!("task#{n} asked for by {by}"))
                    } else {
                        let n = crate::tasks::number(text("task")?)
                            .ok_or((rpc::INVALID_PARAMS, "a task is task#<n>".to_owned()))?;
                        let task = all.get_mut(n).map_err(refused)?;
                        let what = match method {
                            "task_update" => {
                                if let Some(title) =
                                    optional("title").filter(|title| !title.trim().is_empty())
                                {
                                    task.title = title.trim().to_owned();
                                }
                                if let Some(body) = optional("text") {
                                    task.text = body.trim().to_owned();
                                }
                                if params.get("for").is_some() {
                                    task.assignee = optional("for").filter(|who| !who.trim().is_empty());
                                }
                                if source.is_some() {
                                    task.source = source.clone();
                                }
                                format!("task#{n} changed by {by}")
                            }
                            "task_claim" => {
                                let agent = agent.clone().ok_or((
                                    rpc::INVALID_PARAMS,
                                    "give `agent`: who takes it up".to_owned(),
                                ))?;
                                if task.state != "open" {
                                    return Err(refused(format!(
                                        "task#{n} is {}: reopen it first",
                                        task.state
                                    )));
                                }
                                match &task.claimed_by {
                                    Some(other) if *other != agent => {
                                        return Err(refused(format!(
                                            "task#{n} is taken up by {other} already"
                                        )))
                                    }
                                    _ => task.claimed_by = Some(agent.clone()),
                                }
                                format!("{agent} takes up task#{n}")
                            }
                            "task_comment" => {
                                let body = text("text")?.trim().to_owned();
                                if body.chars().filter(|c| c.is_alphabetic()).count() < 2 {
                                    return Err(refused("a comment needs a few words".to_owned()));
                                }
                                let said_by = match &agent {
                                    Some(agent) => format!("{agent} for {by}"),
                                    None => by.clone(),
                                };
                                let next = task.comments.iter().map(|c| c.n).max().unwrap_or(0) + 1;
                                task.comments.push(crate::tasks::TaskComment {
                                    n: next,
                                    by: said_by.clone(),
                                    text: body,
                                    at_ms: now,
                                    source: source.clone(),
                                });
                                format!("{said_by} on task#{n}")
                            }
                            "task_done" => match &agent {
                                // An agent says what it thinks; the person decides.
                                Some(agent) => {
                                    task.thinks_done = Some(agent.clone());
                                    format!("{agent} thinks task#{n} is done")
                                }
                                None => {
                                    task.state = "done".into();
                                    task.thinks_done = None;
                                    format!("task#{n} done, said {by}")
                                }
                            },
                            "task_reopen" => {
                                task.state = "open".into();
                                task.thinks_done = None;
                                format!("task#{n} reopened by {by}")
                            }
                            _ => {
                                task.state = "dropped".into();
                                format!("task#{n} dropped by {by}")
                            }
                        };
                        task.updated_ms = now;
                        (n, what)
                    };
                    crate::tasks::save(&root, &all).map_err(|why| (rpc::INTERNAL_ERROR, why))?;
                    let kind = match method {
                        "task_add" => "task_added",
                        "task_comment" => "task_comment",
                        "task_done" if agent.is_none() => "task_done",
                        _ => "task_changed",
                    };
                    self.events.emit(kind, None, Some(by), what);
                    let task = all.get(n).ok_or((rpc::INTERNAL_ERROR, "the task is gone".to_owned()))?;
                    to_json(serde_json::to_value(crate::tasks::info(task, &lanes, true)))
                }
                // What agents are told (ADR 0012): kept by Kito, out of
                // the project's files.
                "instructions" => {
                    let got = self.with_repo(|repo| Ok(repo.instructions())).map_err(internal)?;
                    to_json(serde_json::to_value(got))
                }
                // What an assistant reads, Kito's rules first (ADR 0012).
                "instructions_text" => {
                    let assistant = optional("assistant").unwrap_or_else(|| "claude".to_owned());
                    let got = self.with_repo(|repo| Ok(repo.instructions())).map_err(internal)?;
                    let pairs: Vec<(&str, &str)> =
                        got.items.iter().map(|item| (item.name.as_str(), item.text.as_str())).collect();
                    let text =
                        crate::instructions::compose(&pairs, &[crate::instructions::ALL, assistant.as_str()]);
                    Ok(json!({ "assistant": assistant, "text": text }))
                }
                "instructions_history" => {
                    let root = self.with_repo(|repo| Ok(repo.root().to_owned())).map_err(internal)?;
                    Ok(json!(crate::instructions::history(&root)))
                }
                "instructions_set" | "instructions_remove" => {
                    let given = text("name")?;
                    let name =
                        crate::instructions::name_of(given).map_err(|why| (rpc::INVALID_PARAMS, why))?;
                    let root = self.with_repo(|repo| Ok(repo.root().to_owned())).map_err(internal)?;
                    let before = crate::instructions::own(&root, &name);
                    let (changed, what) = match request.method.as_str() {
                        "instructions_set" => {
                            let body = params.get("text").and_then(Value::as_str).unwrap_or_default();
                            let changed = crate::instructions::set(&root, &name, body)
                                .map_err(|why| (rpc::INTERNAL_ERROR, why))?;
                            let what = match body.trim().is_empty() {
                                true => format!("{name} of the kitchen is not told here"),
                                false => format!("{name} changed"),
                            };
                            (changed, what)
                        }
                        _ => {
                            let changed = crate::instructions::remove(&root, &name)
                                .map_err(|why| (rpc::INTERNAL_ERROR, why))?;
                            (changed, format!("{name} removed"))
                        }
                    };
                    if changed {
                        let after = crate::instructions::own(&root, &name);
                        // Who: the person a server says, or whoever works on this machine.
                        let by = person_name(params)
                            .or_else(|| self.with_repo(|repo| Ok(repo.identity.name.clone())).ok())
                            .filter(|by| !by.is_empty())
                            .unwrap_or_else(|| "this machine".to_owned());
                        crate::instructions::note(&root, &by, &name, before.as_deref(), after.as_deref());
                        self.events.emit("instructions", None, person_name(params), what);
                    }
                    Ok(json!({ "name": name, "changed": changed }))
                }
                "protect" => {
                    let on = params.get("on").and_then(Value::as_bool).unwrap_or(true);
                    let changed = self.with_repo(|repo| repo.protect(on)).map_err(internal)?;
                    if changed {
                        let what = if on {
                            "the main lane is protected"
                        } else {
                            "the main lane is no longer protected"
                        };
                        self.events.emit("protection", Some(LaneId::MAIN.to_string()), None, what.to_owned());
                    }
                    Ok(json!({ "protected": on, "changed": changed }))
                }
                // Asked by whoever wants to know that the daemon is there.
                "ping" => Ok(
                    json!({ "version": env!("CARGO_PKG_VERSION"), "api": rpc::API, "pid": std::process::id() }),
                ),
                // Plugins (ADR 0005).
                "plugins" => {
                    let mut plugins = self.with_repo(|repo| Ok(repo.plugins())).map_err(internal)?;
                    let failures = self.plugin_failures.lock().unwrap_or_else(|e| e.into_inner());
                    for plugin in &mut plugins {
                        plugin.last_failure = failures.get(&plugin.name).cloned();
                    }
                    to_json(serde_json::to_value(plugins))
                }
                "plugin_enable" => {
                    let name = text("name")?;
                    let permissions: Option<Vec<crate::plugins::Permission>> = match params.get("permissions")
                    {
                        None | Some(Value::Null) => None,
                        Some(given) => Some(serde_json::from_value(given.clone()).map_err(|_| {
                            (
                                rpc::INVALID_PARAMS,
                                "permissions are read, write, integrate or admin".to_owned(),
                            )
                        })?),
                    };
                    let enabled =
                        self.with_repo(|repo| repo.enable_plugin(name, permissions)).map_err(internal)?;
                    self.plugins_changed.fetch_add(1, Ordering::Relaxed);
                    to_json(serde_json::to_value(enabled))
                }
                "plugin_disable" => {
                    let name = text("name")?;
                    let purge = params.get("purge").and_then(Value::as_bool).unwrap_or(false);
                    let changed =
                        self.with_repo(|repo| repo.disable_plugin(name, purge)).map_err(internal)?;
                    self.plugins_changed.fetch_add(1, Ordering::Relaxed);
                    self.plugin_failures.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
                    Ok(json!({ "name": name, "changed": changed }))
                }
                // A plugin asks its counterpart on a server (ADR 0007):
                // this repository is on none.
                "plugin_call" => Err((
                    rpc::REFUSED,
                    "this repository is not connected to a server: there is no plugin there to ask"
                        .to_owned(),
                )),
                "plugin_key" => {
                    let name = text("name")?;
                    let key = self.with_repo(|repo| repo.plugin_key(name)).map_err(internal)?;
                    Ok(json!({ "name": name, "key": key }))
                }
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
                "why_graph" => {
                    let lane = lane_or_main()?;
                    let target = text("target")?;
                    let uses = params.get("uses").and_then(Value::as_bool).unwrap_or(false);
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| {
                            let graph = repo.why_graph(lane, target, uses)?;
                            // A lane that asked was told: no edit of its
                            // is held back for these reasons.
                            repo.tell(lane, &graph.reasons);
                            Ok(graph)
                        })
                        .map_err(internal)?,
                    ))
                }
                "reasons" => {
                    let about = optional("about").unwrap_or_default();
                    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.reasons(&about, limit)).map_err(internal)?,
                    ))
                }
                // Notes on code (ADR 0003).
                "add_note" => {
                    let lane = lane_or_main()?;
                    let (on, note) = (text("on")?, text("text")?);
                    let person = params.get("on_behalf_of").and_then(person_of);
                    // Written by an agent for a person, or by the person.
                    let (by, person) = match optional("agent").filter(|agent| !agent.trim().is_empty()) {
                        Some(agent) => (Some(kito_core::Author::agent(agent.trim())), person),
                        None => (person, None),
                    };
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.add_note(lane, on, note, by, person)).map_err(internal)?,
                    ))
                }
                "notes" => {
                    let lane = lane_or_main()?;
                    let on = optional("on");
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.notes(lane, on.as_deref())).map_err(internal)?,
                    ))
                }
                "remove_note" => {
                    let id: kito_core::NoteId = text("note")?
                        .parse()
                        .map_err(|e: kito_core::IdParseError| (rpc::INVALID_PARAMS, e.to_string()))?;
                    let by = params.get("on_behalf_of").and_then(person_of);
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.remove_note(id, by)).map_err(internal)?,
                    ))
                }
                // Comments on a lane (ADR 0004).
                "comment" => {
                    let lane = lane()?;
                    let said = text("text")?;
                    let on = optional("on");
                    let reply_to = optional("reply_to")
                        .map(|id| id.parse::<kito_core::CommentId>())
                        .transpose()
                        .map_err(|e: kito_core::IdParseError| (rpc::INVALID_PARAMS, e.to_string()))?;
                    let person = params.get("on_behalf_of").and_then(person_of);
                    // Written by an agent for a person, or by the person.
                    let (by, person) = match optional("agent").filter(|agent| !agent.trim().is_empty()) {
                        Some(agent) => (Some(kito_core::Author::agent(agent.trim())), person),
                        None => (person, None),
                    };
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| {
                            repo.add_comment(lane, on.as_deref(), said, by, person, reply_to)
                        })
                        .map_err(internal)?,
                    ))
                }
                "comments" => {
                    let lane = lane()?;
                    let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.comments(lane, all)).map_err(internal)?,
                    ))
                }
                "resolve_comment" => {
                    let id: kito_core::CommentId = text("comment")?
                        .parse()
                        .map_err(|e: kito_core::IdParseError| (rpc::INVALID_PARAMS, e.to_string()))?;
                    let by = params.get("on_behalf_of").and_then(person_of);
                    let (person, integrator) = (person_name(params), integrates(params));
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.resolve_comment(id, by, person.as_deref(), integrator))
                            .map_err(internal)?,
                    ))
                }
                // Verdicts on a lane (ADR 0004).
                "verdict" => {
                    let lane = lane()?;
                    let kind = match text("verdict")? {
                        "approve" | "approved" => kito_core::VerdictKind::Approved,
                        "request_changes" | "changes_requested" => kito_core::VerdictKind::ChangesRequested,
                        "withdraw" | "withdrawn" => kito_core::VerdictKind::Withdrawn,
                        other => {
                            return Err((
                                rpc::INVALID_PARAMS,
                                format!("{other:?} is not a verdict: approve, request_changes or withdraw"),
                            ))
                        }
                    };
                    let said = optional("text");
                    let person = params.get("on_behalf_of").and_then(person_of);
                    // Given by an agent for a person, or by the person.
                    let (by, person) = match optional("agent").filter(|agent| !agent.trim().is_empty()) {
                        Some(agent) => (Some(kito_core::Author::agent(agent.trim())), person),
                        None => (person, None),
                    };
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.give_verdict(lane, kind, said.as_deref(), by, person))
                            .map_err(internal)?,
                    ))
                }
                "review" => {
                    let lane = lane()?;
                    to_json(serde_json::to_value(self.with_repo(|repo| repo.review(lane)).map_err(internal)?))
                }
                "checkpoint" => {
                    let label = text("label")?;
                    let done = self.with_repo(|repo| repo.checkpoint(label)).map_err(internal)?;
                    self.push_wanted.notify_one();
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
                // A permanent lane, `staging` (ADR 0001).
                "create_lane" => {
                    let name = text("name")?;
                    let from = optional("from");
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| {
                            let from = from.as_deref().map(|spec| repo.lane_of(spec)).transpose()?;
                            repo.create_lane(name, from)
                        })
                        .map_err(internal)?,
                    ))
                }
                "catch_up" => {
                    let lane = lane()?;
                    let written = self.with_repo(|repo| repo.catch_up(lane)).map_err(internal)?;
                    Ok(json!({ "lane": lane.to_string(), "files": written }))
                }
                "open_lane" => {
                    let agent = text("agent")?;
                    let (task, base) = (optional("task"), optional("base"));
                    let intent = optional("intent").unwrap_or_default();
                    let scope = scope_of(params);
                    let (on, target) = (optional("on"), optional("target"));
                    let said = self.said_of(params);
                    let (opened, overlaps, (why, why_note)) = self
                        .with_repo(|repo| {
                            // Building on a lane reads it; landing on it is
                            // for whoever may change it.
                            let stack = crate::lanes::Stack {
                                on: on.as_deref().map(|spec| repo.lane_of(spec)).transpose()?,
                                target: target.as_deref().map(|spec| repo.lane_of(spec)).transpose()?,
                            };
                            let opened =
                                repo.open_lane_on(agent, task, base.as_deref(), &intent, &scope, stack)?;
                            let lane = repo.lane_of(&opened.id)?;
                            // What its agent runs in, when that side said it (ADR 0010).
                            let opened = match said.is_empty() {
                                true => opened,
                                false => {
                                    repo.lane_assistant_said(lane, said.clone())?;
                                    repo.lane_info(lane)?
                                }
                            };
                            // The person a server authenticated: the lane is theirs.
                            let opened = match params.get("on_behalf_of").and_then(person_of) {
                                Some(person) => {
                                    repo.set_lane_owner(lane, person)?;
                                    repo.lane_info(lane)?
                                }
                                None => opened,
                            };
                            // Who is already on the same code: told now, and
                            // not again as a notice. And why the code it
                            // declared is the way it is.
                            let overlaps = repo.take_notices(lane)?;
                            Ok((opened, overlaps, repo.why_of_scope(lane, &scope)))
                        })
                        .map_err(internal)?;
                    let mut opened =
                        serde_json::to_value(opened).map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
                    opened["overlaps"] = to_json(serde_json::to_value(overlaps))?;
                    opened["why"] = to_json(serde_json::to_value(why))?;
                    if let Some(note) = why_note {
                        opened["why_note"] = Value::String(note);
                    }
                    Ok(opened)
                }
                "extend_scope" => {
                    let lane = lane()?;
                    owned(lane)?;
                    let scope = scope_of(params);
                    let (declared, overlaps, (why, why_note)) = self
                        .with_repo(|repo| {
                            let declared = repo.extend_scope(lane, &scope)?;
                            let overlaps = repo.take_notices(lane)?;
                            Ok((declared, overlaps, repo.why_of_scope(lane, &scope)))
                        })
                        .map_err(internal)?;
                    let mut extended = json!({
                        "lane": lane.to_string(),
                        "scope": to_json(serde_json::to_value(declared))?,
                        "overlaps": to_json(serde_json::to_value(overlaps))?,
                        "why": to_json(serde_json::to_value(why))?,
                    });
                    if let Some(note) = why_note {
                        extended["why_note"] = Value::String(note);
                    }
                    Ok(extended)
                }
                "before_edit" => {
                    let path = crate::scope::clean(text("path")?).map_err(internal)?;
                    let lane = match params.get("lane").and_then(Value::as_str) {
                        Some(spec) => Some(
                            spec.trim()
                                .parse::<LaneId>()
                                .map_err(|_| (rpc::INVALID_PARAMS, format!("{spec:?} is not a lane")))?,
                        ),
                        None => None,
                    };
                    let lines: Vec<(u32, u32)> = params
                        .get("lines")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|pair| {
                            let pair = pair.as_array()?;
                            Some((pair.first()?.as_u64()? as u32, pair.get(1)?.as_u64()? as u32))
                        })
                        .collect();
                    let whole = params.get("whole").and_then(Value::as_bool).unwrap_or(false);
                    // The hook that asks says what it runs in: the lane the
                    // edit is in keeps it (ADR 0010). A lane that is gone
                    // has nobody at work in it.
                    let said = self.said_of(params);
                    if let (Some(lane), false) = (lane, said.is_empty()) {
                        let _ = self.with_repo(|repo| repo.lane_assistant_said(lane, said.clone()));
                    }
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.before_edit(lane, &path, &lines, whole))
                            .map_err(internal)?,
                    ))
                }
                // The session of an assistant says what it runs on, before
                // it is in any lane: the lanes it then edits in are told
                // (ADR 0010). Nothing of the repository changes.
                "assistant_session" => {
                    let said: kito_core::AssistantSaid = params
                        .get("assistant")
                        .and_then(|said| serde_json::from_value(said.clone()).ok())
                        .unwrap_or_default();
                    self.session_said(text("session")?, said);
                    Ok(json!({}))
                }
                "pull" => to_json(serde_json::to_value(self.pull().map_err(|why| (rpc::REFUSED, why))?)),
                "reasons_behind" => {
                    let lane = lane()?;
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.reasons_behind(lane)).map_err(internal)?,
                    ))
                }
                "after_shell" => {
                    let since = params.get("since_ms").and_then(Value::as_u64).unwrap_or(0);
                    let lanes: Vec<LaneId> = params
                        .get("lanes")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter_map(|spec| spec.trim().parse().ok())
                        .collect();
                    let main = params.get("main").and_then(Value::as_bool).unwrap_or(true);
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.after_shell(since, &lanes, main)).map_err(internal)?,
                    ))
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
                    // Asked for a lane by the program its agent runs in,
                    // which says what it is (ADR 0010).
                    let said = self.said_of(params);
                    if !said.is_empty() {
                        let _ = self.with_repo(|repo| repo.lane_assistant_said(lane, said.clone()));
                    }
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.take_notices(lane)).map_err(internal)?,
                    ))
                }
                "close_lane" => {
                    let lane = lane()?;
                    owned(lane)?;
                    self.writers.lock().unwrap_or_else(|e| e.into_inner()).remove(&lane);
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| repo.close_lane(lane)).map_err(internal)?,
                    ))
                }
                // Review of a lane (ADR 0004): what it changes.
                "lane_diff" => {
                    let lane = lane()?;
                    let paths: Vec<String> = params
                        .get("paths")
                        .and_then(Value::as_array)
                        .map(|paths| paths.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                        .unwrap_or_default();
                    to_json(serde_json::to_value(
                        self.with_repo(|repo| {
                            // As the working copy is now, not as it was last seen.
                            repo.scan_lane(lane)?;
                            repo.lane_diff(lane, &paths)
                        })
                        .map_err(internal)?,
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
                    // What was said about it and is not resolved (ADR 0004).
                    let comments = self.with_repo(|repo| repo.comments(lane, false)).map_err(internal)?;
                    status["comments"] = to_json(serde_json::to_value(comments))?;
                    Ok(status)
                }
                "set_intent" => {
                    let lane = lane_or_main()?;
                    owned(lane)?;
                    let summary = text("summary")?;
                    let (task, prompt) = (optional("task"), optional("prompt"));
                    let intent = self
                        .with_repo(|repo| repo.set_intent(lane, summary, task, prompt.as_deref()))
                        .map_err(internal)?;
                    Ok(json!({ "intent": intent, "lane": lane.to_string() }))
                }
                "rename_symbol" => {
                    let lane = lane_or_main()?;
                    owned(lane)?;
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
                    owned(lane)?;
                    let asker = params.get("on_behalf_of").and_then(person_of);
                    // Landing on another lane waits for no checks, and so
                    // for no lane landing on the main lane: it is done now,
                    // by whoever may change that lane (ADR 0001).
                    let (target, permanent) = self
                        .with_repo(|repo| {
                            let target = repo.target_of(lane)?;
                            // Who may land on a permanent lane, when it says.
                            repo.may_land_on(target, person_name(params).as_deref())?;
                            Ok((target, repo.is_permanent(target)))
                        })
                        .map_err(internal)?;
                    if !permanent {
                        let person = person_name(params);
                        let result = self
                            .with_repo(|repo| {
                                repo.may_change(target, person.as_deref(), integrates(params))?;
                                repo.integrate_leaving_index(lane)
                            })
                            .map_err(internal)?;
                        self.integration_ended(&result);
                        let job = self.queue.done_now(result);
                        return Ok(json!({ "job": job.to_string(), "lane": lane.to_string() }));
                    }
                    // Asked by somebody who may not integrate, the merge
                    // and the checks are done and the lane then waits.
                    let mut holds = self.holds.lock().unwrap_or_else(|e| e.into_inner());
                    let job = self.queue.submit(lane);
                    if let Some(person) = asker {
                        holds.insert(job, (person, integrates(params)));
                    }
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
                    if let Some(lane) = self.with_repo(|repo| Ok(repo.conflict_lane(id))).map_err(internal)? {
                        owned(lane)?;
                    }
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
        // As the answer names it (`lane/1`), else as it was asked.
        let lane = text(value, "lane").or_else(|| text(params, "lane"));
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
            // An edit that was not made because of where it was: a team
            // that watches sees an agent off its lane. One held back for
            // its reasons is the ordinary course of things, and is not told.
            "before_edit" => {
                let path = text(params, "path").unwrap_or_default();
                let what = match text(value, "kind").as_deref() {
                    Some("main") => format!("{path}: an edit in the main directory, not made"),
                    Some("out_of_scope") => {
                        format!("{}: outside the scope, not made", list(value, "outside"))
                    }
                    _ => return,
                };
                ("edit_refused", lane.or(Some("main".to_owned())), person, what)
            }
            "set_intent" => ("intent", lane, person, text(params, "summary").unwrap_or_default()),
            "add_note" => {
                let what = format!(
                    "{}: {}",
                    text(value, "on").unwrap_or_default(),
                    text(value, "text").unwrap_or_default()
                );
                ("noted", lane, person, what)
            }
            "comment" => {
                let on = text(value, "on").map(|on| format!("{on}: ")).unwrap_or_default();
                ("commented", lane, person, format!("{on}{}", text(value, "text").unwrap_or_default()))
            }
            "resolve_comment" => ("comment_resolved", lane, person, text(value, "id").unwrap_or_default()),
            "verdict" => {
                let words = text(params, "text").map(|words| format!(": {words}")).unwrap_or_default();
                let kind = match text(params, "verdict").unwrap_or_default().as_str() {
                    "approve" | "approved" => "approved",
                    "request_changes" | "changes_requested" => "changes requested",
                    _ => "verdict withdrawn",
                };
                ("reviewed", lane, person, format!("{kind}{words}"))
            }
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
            kito_integrate::Outcome::Integrated => match &result.landed_on {
                Some(target) => format!("landed on {target}"),
                None => result.checkpoint.clone().unwrap_or_default(),
            },
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
async fn integrate(
    daemon: &Arc<Daemon>,
    lane: LaneId,
    asker: Option<(kito_core::Author, bool)>,
) -> IntegrationResult {
    // Who may not integrate has the lane held; who may is named as the
    // one who let it in.
    let (hold, approver) = match asker {
        Some((person, true)) => (None, Some(person)),
        Some((person, false)) => (Some(person), None),
        None => (None, None),
    };
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
            Some(checks) => Some(checks.run().await),
            None => None,
        };
        let (worker, held, approved_by) = (daemon.clone(), hold.clone(), approver.clone());
        let finished = tokio::task::spawn_blocking(move || {
            worker.with_repo(|repo| repo.finish_integration_for(*prepared, report, held, approved_by))
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

/// How long a push to the git remote is given.
const PUSH_WITHIN: Duration = Duration::from_secs(120);

/// Pushes the branch to the git remote the repository names, each time a
/// checkpoint was made: never forced, off the path of whoever asked for
/// the checkpoint. What fails is said, and goes with the next push.
async fn push_loop(daemon: Arc<Daemon>) {
    loop {
        daemon.push_wanted.notified().await;
        let worker = daemon.clone();
        let asked = tokio::task::spawn_blocking(move || {
            worker.with_repo(|repo| {
                let git = &repo.config().git;
                Ok((
                    git.push.clone(),
                    git.branch.clone(),
                    repo.status()?.last_checkpoint,
                    repo.permanent_branches(),
                ))
            })
        })
        .await;
        let Ok(Ok((Some(remote), branch, checkpoint, permanent))) = asked else { continue };
        // What the remote has that the main lane does not comes in first:
        // commits made with git by people who do not work through Kito.
        let worker = daemon.clone();
        let pulled = tokio::task::spawn_blocking(move || worker.pull()).await;
        let mut checkpoint = checkpoint;
        if let Ok(Ok(outcome)) = &pulled {
            if let Some(lane) = &outcome.lane {
                // They wait to be merged: nothing is pushed over them.
                let why = format!("commits of {remote} wait to be merged in {lane}");
                let worker = daemon.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    worker.with_repo(|repo| {
                        repo.set_pushed(None, Some(why));
                        Ok(())
                    })
                })
                .await;
                continue;
            }
            if let Some(merged) = outcome.result.as_ref().and_then(|result| result.checkpoint.clone()) {
                checkpoint = Some(merged);
            }
        }
        // The branch Kito advances: the one named, or the one HEAD is on.
        let what = branch.map_or_else(|| "HEAD".to_owned(), |branch| format!("refs/heads/{branch}"));
        // With the branches of the permanent lanes (ADR 0001).
        let pushing = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&daemon.root)
            .args(["push", "--quiet", &remote, &what])
            .args(permanent.iter().map(|branch| format!("refs/heads/{branch}")))
            // Nobody is there to type a password.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let error = match tokio::time::timeout(PUSH_WITHIN, pushing).await {
            Ok(Ok(output)) if output.status.success() => None,
            Ok(Ok(output)) => {
                let said = String::from_utf8_lossy(&output.stderr);
                Some(
                    said.lines()
                        .rfind(|line| !line.trim().is_empty())
                        .unwrap_or("git push failed")
                        .trim()
                        .to_owned(),
                )
            }
            Ok(Err(e)) => Some(format!("git: {e}")),
            Err(_) => Some(format!("no answer from {remote} in {} s", PUSH_WITHIN.as_secs())),
        };
        let main = Some(LaneId::MAIN.to_string());
        match &error {
            None => {
                let what = format!("{} to {remote}", checkpoint.clone().unwrap_or_default());
                daemon.events.emit("pushed", main, None, what)
            }
            Some(why) => {
                eprintln!("kitod: git push to {remote}: {why}");
                daemon.events.emit("push_failed", main, None, format!("{remote}: {why}"))
            }
        }
        let worker = daemon.clone();
        let _ = tokio::task::spawn_blocking(move || {
            worker.with_repo(|repo| {
                repo.set_pushed(checkpoint, error);
                Ok(())
            })
        })
        .await;
    }
}

/// Serves the integration requests, one at a time, in order.
/// How long the first request of a batch waits for others made in the
/// same breath, when the repository has checks: little next to the checks.
const GATHER: Duration = Duration::from_millis(15);

/// A request as the queue gave it: the job, the lane, and who asked for
/// it with whether they may integrate.
type Asked = (JobId, LaneId, Option<(kito_core::Author, bool)>);

/// The lanes that are integrated together, served by the daemon: the
/// repository is held only where it is needed, the checks run outside it.
struct Together {
    daemon: Arc<Daemon>,
    runtime: tokio::runtime::Handle,
    jobs: Vec<Asked>,
}

impl Together {
    fn asked(&self, lane: LaneId) -> Option<&Asked> {
        self.jobs.iter().find(|(_, asked, _)| *asked == lane)
    }
}

impl crate::integrate::Bench for Together {
    fn repo<R>(&self, f: impl FnOnce(&mut Repo) -> Result<R>) -> Result<R> {
        self.daemon.with_repo(f)
    }

    fn check(&self, checks: &crate::integrate::Checks) -> kito_integrate::VerificationReport {
        self.runtime.block_on(checks.run())
    }

    fn alone(&self, lane: LaneId) -> IntegrationResult {
        let asker = self.asked(lane).and_then(|(_, _, asker)| asker.clone());
        self.runtime.block_on(integrate(&self.daemon, lane, asker))
    }

    fn approver(&self, lane: LaneId) -> Option<kito_core::Author> {
        let asker = self.asked(lane).and_then(|(_, _, asker)| asker.clone());
        asker.filter(|(_, may)| *may).map(|(person, _)| person)
    }

    fn done(&self, lane: LaneId, result: IntegrationResult) {
        if let Some((job, _, _)) = self.asked(lane) {
            self.daemon.conclude(*job, result);
        }
    }
}

/// What bringing in the commits of the git remote did.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PullOutcome {
    pub remote: String,
    /// How many commits were brought into a lane by this call.
    pub commits: usize,
    /// The lane they are in, when they are not on the main lane yet.
    pub lane: Option<String>,
    /// How the integration of that lane ended.
    pub result: Option<IntegrationResult>,
    /// The same, in a sentence.
    pub message: String,
}

impl Daemon {
    /// Brings in what the git remote has and the main lane does not: it
    /// fetches, replays the commits in a lane, and has the lane
    /// integrated like any other, in its turn. Blocking: not to be called
    /// on the runtime's own threads.
    fn pull(&self) -> std::result::Result<PullOutcome, String> {
        let fail = |e: Error| e.to_string();
        let (remote, branch) = self
            .with_repo(|repo| Ok((repo.config().git.push.clone(), repo.config().git.branch.clone())))
            .map_err(fail)?;
        let remote = remote.ok_or(
            "this repository has no git remote to bring commits from: set one with `[git] push` (on a \
             server, `kito-server repo set <name> --push <remote>`)",
        )?;
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(std::process::Stdio::null())
                .output()
        };
        let said = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_owned();
        // The branch Kito advances: the one named, or the one HEAD is on.
        let branch = match branch {
            Some(branch) => branch,
            None => git(&["symbolic-ref", "--short", "HEAD"])
                .ok()
                .filter(|out| out.status.success())
                .map(|out| said(&out.stdout))
                .ok_or("git's HEAD is on no branch: there is nothing to bring commits to")?,
        };
        let fetching = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["fetch", "--quiet", &remote, &branch])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let nothing = |message: String| PullOutcome {
            remote: remote.clone(),
            commits: 0,
            lane: None,
            result: None,
            message,
        };
        let runtime = tokio::runtime::Handle::current();
        match runtime.block_on(tokio::time::timeout(PUSH_WITHIN, fetching)) {
            Ok(Ok(output)) if output.status.success() => {}
            Ok(Ok(output)) => {
                let complaint = said(&output.stderr);
                // A remote that has no such branch yet has nothing to bring.
                if complaint.contains("couldn't find remote ref") {
                    return Ok(nothing(format!("{remote} has no branch {branch} yet")));
                }
                let last =
                    complaint.lines().rfind(|line| !line.trim().is_empty()).unwrap_or("git fetch failed");
                return Err(format!("{remote}: {}", last.trim()));
            }
            Ok(Err(e)) => return Err(format!("git: {e}")),
            Err(_) => return Err(format!("no answer from {remote} in {} s", PUSH_WITHIN.as_secs())),
        }
        let tip = git(&["rev-parse", "FETCH_HEAD"])
            .ok()
            .filter(|out| out.status.success())
            .map(|out| said(&out.stdout))
            .ok_or("git fetched nothing it can name")?;
        let Some(pull) = self.with_repo(|repo| repo.pull_import(&tip)).map_err(fail)? else {
            return Ok(nothing(format!("nothing to bring in from {remote}")));
        };
        // Integrated like any lane: in its turn, with the checks.
        let job = self.queue.submit(pull.lane);
        let result = loop {
            match self.queue.state(job) {
                Some(JobState::Done { result }) => break *result,
                Some(_) => std::thread::sleep(Duration::from_millis(10)),
                None => return Err("the integration of the commits was lost".to_owned()),
            }
        };
        let lane = pull.lane.to_string();
        let commits = match pull.commits {
            0 => "the commits".to_owned(),
            1 => "1 commit".to_owned(),
            n => format!("{n} commits"),
        };
        let (waits, message) = match result.result {
            kito_integrate::Outcome::Integrated => {
                self.with_repo(|repo| repo.pull_done(pull.lane)).map_err(fail)?;
                let checkpoint = result.checkpoint.clone().unwrap_or_default();
                (false, format!("{commits} of {remote} merged into the main lane as {checkpoint}"))
            }
            kito_integrate::Outcome::NothingToIntegrate => {
                let joined = self.with_repo(|repo| repo.pull_joined(pull.lane)).map_err(fail)?;
                (
                    false,
                    format!(
                        "{commits} of {remote} change nothing the main lane does not have: the two \
                         histories are joined as {joined}"
                    ),
                )
            }
            kito_integrate::Outcome::Conflicts => (
                true,
                format!(
                    "{commits} of {remote} wait in {lane}: they conflict with what the main lane did \
                     (`kito conflicts`, `kito resolve`, then `kito pull` again)"
                ),
            ),
            kito_integrate::Outcome::VerificationFailed => (
                true,
                format!(
                    "{commits} of {remote} wait in {lane}: merged with the main lane they fail the checks \
                     (fix it in the lane, then `kito pull` again)"
                ),
            ),
            _ => (
                true,
                format!(
                    "{commits} of {remote} wait in {lane}: {}",
                    result.message.clone().unwrap_or_else(|| "they could not be integrated".to_owned())
                ),
            ),
        };
        self.events.emit("pulled", Some(LaneId::MAIN.to_string()), None, message.clone());
        if !waits {
            // What was brought in goes back to the remote, merged.
            self.push_wanted.notify_one();
        }
        Ok(PullOutcome {
            remote,
            commits: pull.commits,
            lane: waits.then_some(lane),
            result: Some(result),
            message,
        })
    }

    /// A lane has its answer: whoever watches is told, and whoever waits.
    fn conclude(&self, job: JobId, result: IntegrationResult) {
        self.integration_ended(&result);
        if result.result == kito_integrate::Outcome::Integrated {
            self.push_wanted.notify_one();
        }
        self.queue.finish(job, result);
    }
}

async fn integration_loop(daemon: Arc<Daemon>) {
    loop {
        // With checks to wait for, the lanes that ask meanwhile are taken
        // together when their turn comes.
        let most = daemon.with_repo(|repo| Ok(repo.batch_limit())).unwrap_or(1);
        let jobs = daemon.queue.next_batch(most, GATHER).await;
        let asked: Vec<Asked> = {
            let mut holds = daemon.holds.lock().unwrap_or_else(|e| e.into_inner());
            jobs.into_iter().map(|(job, lane)| (job, lane, holds.remove(&job))).collect()
        };
        // A lane asked for by somebody who may not integrate is merged and
        // checked, then waits: that is done by itself.
        let (together, mut alone): (Vec<Asked>, Vec<Asked>) =
            asked.into_iter().partition(|(_, _, asker)| !matches!(asker, Some((_, false))));
        match together.len() {
            0 | 1 => alone.extend(together),
            _ => {
                let bench = Together {
                    daemon: daemon.clone(),
                    runtime: tokio::runtime::Handle::current(),
                    jobs: together,
                };
                let served = tokio::task::spawn_blocking(move || {
                    let lanes: Vec<LaneId> = bench.jobs.iter().map(|(_, lane, _)| *lane).collect();
                    crate::integrate::integrate_batch(&bench, &lanes);
                    bench
                })
                .await;
                // Whatever happened, nobody is left without an answer.
                if let Ok(bench) = served {
                    for (job, lane, _) in &bench.jobs {
                        if !matches!(daemon.queue.state(*job), Some(kito_integrate::JobState::Done { .. })) {
                            let failed =
                                IntegrationResult::failed(lane.to_string(), "the integration did not end");
                            daemon.conclude(*job, failed);
                        }
                    }
                }
            }
        }
        alone.sort_by_key(|(job, _, _)| *job);
        for (job, lane, asker) in alone {
            let result = integrate(&daemon, lane, asker).await;
            daemon.conclude(job, result);
        }
        // The lanes have their answers: now git's index, left behind by
        // the export of the integrations, is brought up to the commit.
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

/// Runs a plugin for one event: `kito-<name> event`, with the event as
/// JSON on its input. `Err`: why it failed.
fn run_for_event(run: &crate::plugins::EventRun, event: &rpc::Event) -> std::result::Result<(), String> {
    let told = serde_json::to_vec(event).unwrap_or_default();
    crate::plugins::run_plugin(&run.program, &["event"], &run.root, &run.key, &told, run.timeout).map(drop)
}

/// Tells the plugins what happens: each is run for the events it listed,
/// one at a time and in order, after the fact. Nothing waits for them.
fn plugin_events(daemon: &Arc<Daemon>) {
    use std::sync::mpsc;
    type Told = (crate::plugins::EventRun, rpc::Event);
    let (_, mut seen) = daemon.events.after(None, Duration::ZERO);
    let mut workers: std::collections::HashMap<String, mpsc::Sender<Told>> = Default::default();
    // Which plugins listen is read again when it may have changed, not
    // for every save.
    let mut runs: Vec<crate::plugins::EventRun> = Vec::new();
    let mut read_at: Option<(Instant, u64)> = None;
    loop {
        let (events, next) = daemon.events.after(Some(seen), Duration::from_secs(5));
        seen = next;
        for event in events {
            let changed = daemon.plugins_changed.load(Ordering::Relaxed);
            if read_at.is_none_or(|(at, then)| then != changed || at.elapsed() > Duration::from_secs(2)) {
                runs = match daemon.with_repo(|repo| Ok(repo.event_runs())) {
                    Ok(runs) => runs,
                    // The repository is closed: the daemon is going.
                    Err(_) => return,
                };
                read_at = Some((Instant::now(), changed));
            }
            for run in runs.iter().filter(|run| run.kinds.contains(&event.kind)) {
                // Its own failure is not told back to it, over and over.
                if event.kind == "plugin_failed" && event.by.as_deref() == Some(run.plugin.as_str()) {
                    continue;
                }
                let worker = workers.entry(run.plugin.clone()).or_insert_with(|| {
                    let (send, receive) = mpsc::channel::<Told>();
                    let daemon = daemon.clone();
                    let _ = std::thread::Builder::new().name("plugin-event".into()).spawn(move || {
                        for (run, event) in receive {
                            let outcome = run_for_event(&run, &event);
                            let mut failures =
                                daemon.plugin_failures.lock().unwrap_or_else(|e| e.into_inner());
                            match outcome {
                                Ok(()) => drop(failures.remove(&run.plugin)),
                                Err(why) => {
                                    let what = format!("on `{}`: {why}", event.kind);
                                    failures.insert(run.plugin.clone(), what.clone());
                                    drop(failures);
                                    daemon.events.emit(
                                        "plugin_failed",
                                        event.lane.clone(),
                                        Some(run.plugin.clone()),
                                        what,
                                    );
                                }
                            }
                        }
                    });
                    send
                });
                let _ = worker.send((run.clone(), event.clone()));
            }
        }
    }
}

async fn serve_connection(daemon: Arc<Daemon>, stream: Box<dyn crate::ipc::Connection>) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    // Whoever speaks first says a key: the socket's, or the one of a
    // plugin this repository enabled (ADR 0005).
    let mut access = daemon.key.is_none().then_some(crate::plugins::Access::Owner);
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Some(who) = &access else {
            match daemon.who_said(&line) {
                Some(who) => {
                    access = Some(who);
                    continue;
                }
                None => {
                    let id = serde_json::from_str::<Request>(&line).map_or(0, |request| request.id);
                    let mut bytes =
                        serde_json::to_vec(&Response::err(id, rpc::REFUSED, rpc::NO_KEY)).unwrap_or_default();
                    bytes.push(b'\n');
                    let _ = writer.write_all(&bytes).await;
                    break;
                }
            }
        };
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => match who.refusal(&request.method) {
                Some(why) => Response::err(request.id, rpc::REFUSED, why),
                None => {
                    let worker = daemon.clone();
                    let id = request.id;
                    tokio::task::spawn_blocking(move || worker.handle(&request))
                        .await
                        .unwrap_or_else(|e| Response::err(id, rpc::INTERNAL_ERROR, e.to_string()))
                }
            },
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
                            let path = kito_core::paths::portable_path(&path);
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
    let mut listener = crate::ipc::Listener::bind(&root).map_err(Error::io(&socket))?;

    let daemon = Arc::new(Daemon {
        root: root.clone(),
        socket: socket.clone(),
        key: rpc::ensure_socket_key(&root).ok(),
        plugin_failures: Mutex::default(),
        plugins_changed: AtomicU64::new(0),
        sessions: Mutex::default(),
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
        holds: Mutex::default(),
        push_wanted: Notify::new(),
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
    let pusher = tokio::spawn(push_loop(daemon.clone()));
    // The plugins that are run for events (ADR 0005): on a thread of its
    // own, which ends with the process.
    let teller = daemon.clone();
    let _ = std::thread::Builder::new().name("plugin-events".into()).spawn(move || plugin_events(&teller));
    let mut terminated = terminate_signal()?;

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    tokio::spawn(serve_connection(daemon.clone(), stream));
                }
                Err(e) => eprintln!("kitod: accept: {e}"),
            },
            _ = daemon.shutdown.notified() => break,
            _ = terminated.recv() => break,
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
    pusher.abort();
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

/// What the person a request is made for may do, when a server said so:
/// `read`, `write` or `integrate`.
fn role_of(params: &Value) -> Option<&str> {
    params.get("on_behalf_of")?.get("role")?.as_str()
}

/// `true` unless a server said the person may not integrate. On the
/// machine of the repository whoever can reach it may.
fn integrates(params: &Value) -> bool {
    role_of(params).is_none_or(|role| role == "integrate" || role == "maintain")
}
