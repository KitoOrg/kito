//! The daemon of a directory connected to a repository on a server.
//!
//! It answers the same requests as the daemon of a repository, on the same
//! kind of socket, by asking the server; and it keeps on this machine the
//! working copies of the lanes that are worked on or followed from here
//! (see [`crate::mirror`]). A lane opened from here is edited here: its
//! saves reach the server on their own, and what the server changes in the
//! lane comes back.

use crate::client::{ClientError, Remote};
use crate::mirror::{dir_name, unreachable, Mirror, MirrorInfo, Upstream};
use crate::rpc::{self, Request, Response};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A working copy with no watcher, or with saves that did not get
/// through, is looked at whole this often.
const RESCAN: Duration = Duration::from_secs(2);

/// A working copy is looked at whole this often whatever the watcher says.
const RESCAN_WATCHED: Duration = Duration::from_secs(30);

/// A file closed after being written is looked at after this long.
const CLOSED: Duration = Duration::from_millis(10);

/// A file being written is looked at when nothing happened to it for this long.
const QUIET: Duration = Duration::from_millis(30);

/// How long the server is given to say that something happened.
const EVENTS_WAIT_MS: u64 = 20_000;

/// Events after which the files of a lane may have changed.
const CHANGES_FILES: &[&str] =
    &["saved", "integrated", "renamed", "conflicts", "verification_failed", "failed", "conflict_resolved"];

type Answer = Result<Value, (i64, String)>;

struct Running {
    mirror: Arc<Mirror>,
    /// Dropped with the mirror: the watcher of its working copy.
    _watcher: Option<notify::RecommendedWatcher>,
}

struct Daemon {
    root: PathBuf,
    /// What a client says first on the socket.
    key: Option<String>,
    remote: Remote,
    /// This machine, as the server knows the copy that writes in a lane.
    writer: String,
    mirrors: Mutex<BTreeMap<String, Running>>,
    /// Connections to the server that nobody is using: a client that
    /// arrives takes one instead of opening its own.
    idle: Mutex<Vec<Upstream>>,
}

/// How many connections to the server are kept for the next client.
const IDLE_KEPT: usize = 4;

fn failed(error: ClientError) -> (i64, String) {
    match error {
        ClientError::Rpc(error) => (error.code, error.message),
        e => (rpc::INTERNAL_ERROR, e.to_string()),
    }
}

fn host_name() -> String {
    let read = |path: &str| std::fs::read_to_string(path).ok().map(|name| name.trim().to_owned());
    read("/proc/sys/kernel/hostname")
        .or_else(|| read("/etc/hostname"))
        .or_else(|| {
            let output = std::process::Command::new("hostname").output().ok()?;
            Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "client".to_owned())
}

/// The name this directory writes under, made once and kept.
fn writer_id(kito: &Path) -> String {
    let file = kito.join("client-id");
    if let Ok(id) = std::fs::read_to_string(&file) {
        if !id.trim().is_empty() {
            return id.trim().to_owned();
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let seed = format!("{}:{nanos}:{}", std::process::id(), kito.display());
    let id = format!("{}:{}", host_name().replace(':', "-"), kito_core::Hash::of(seed.as_bytes()).short());
    let _ = std::fs::write(&file, &id);
    id
}

/// The name of a lane as the server writes it: `lane/3`, `lane/main`.
fn lane_name(spec: &str) -> Option<String> {
    spec.parse::<kito_core::LaneId>().ok().map(|lane| lane.to_string())
}

/// Finds the lane an answer is about.
fn lane_in(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) => {
            map.get("lane").and_then(Value::as_str).or_else(|| map.values().find_map(lane_in))
        }
        Value::Array(items) => items.iter().find_map(lane_in),
        _ => None,
    }
}

impl Daemon {
    fn mirrors(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Running>> {
        self.mirrors.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The mirrors of the lanes still open.
    fn open(&self) -> Vec<Arc<Mirror>> {
        self.mirrors()
            .values()
            .map(|running| running.mirror.clone())
            .filter(|mirror| !mirror.closed())
            .collect()
    }

    /// The mirror of a lane named as the server names it: `lane/3`, `3`,
    /// `main`.
    fn mirror(&self, spec: &str) -> Option<Arc<Mirror>> {
        let lane = lane_name(spec)?;
        self.mirrors().get(&lane).map(|running| running.mirror.clone())
    }

    fn mirror_asked(&self, params: &Value) -> Option<Arc<Mirror>> {
        params.get("lane").and_then(Value::as_str).and_then(|spec| self.mirror(spec))
    }

    /// Starts what keeps a mirror in step: the watcher of its working copy
    /// and the thread that sends its saves.
    fn start(self: &Arc<Self>, mirror: Mirror) -> Arc<Mirror> {
        let mirror = Arc::new(mirror);
        let (seen, work) = (mirror.clone(), mirror.work.clone());
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            use notify::event::{AccessKind, AccessMode, ModifyKind};
            use notify::EventKind;
            let event = match event {
                Ok(event) if !event.need_rescan() => event,
                _ => return seen.touched_all(),
            };
            // A file put in place by a rename, or removed, is looked at
            // at once. One closed after being written is given a moment:
            // many programs write a file beside the real one and rename
            // it, and that file is nobody's save. One still being written
            // is looked at when it has been quiet for a while.
            let wait = match event.kind {
                EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => Duration::ZERO,
                EventKind::Access(AccessKind::Close(AccessMode::Write)) => CLOSED,
                EventKind::Access(_) => return,
                _ => QUIET,
            };
            for path in &event.paths {
                match path.strip_prefix(&work) {
                    // What Kito keeps for itself is no file of the lane.
                    Ok(rel) if rel.starts_with(kito_store::STORE_DIR) => {}
                    Ok(rel) if !rel.as_os_str().is_empty() => {
                        seen.touched(kito_core::paths::portable_path(rel), wait)
                    }
                    _ => {}
                }
            }
        })
        .and_then(|mut watcher| {
            use notify::Watcher;
            watcher.watch(&mirror.work, notify::RecursiveMode::Recursive)?;
            Ok(watcher)
        });
        if let Err(e) = &watcher {
            eprintln!(
                "kitod: {}: no watcher ({e}): its working copy is looked at every {RESCAN:?}",
                mirror.lane
            );
        }
        let (daemon, sending, watched) = (self.clone(), mirror.clone(), watcher.is_ok());
        std::thread::spawn(move || daemon.send(&sending, watched));
        self.mirrors()
            .insert(mirror.lane.clone(), Running { mirror: mirror.clone(), _watcher: watcher.ok() });
        // Whatever was saved while no daemon was looking.
        mirror.touched_all();
        mirror
    }

    /// Sends the saves of a mirror as the watcher reports them, for as
    /// long as the mirror is there.
    fn send(&self, mirror: &Mirror, watched: bool) {
        let mut up = Upstream::new(&self.remote);
        let mut waiting = false;
        loop {
            let limit = if waiting || !watched { RESCAN } else { RESCAN_WATCHED };
            let news = mirror.wait_for_news(limit);
            if mirror.stop.load(Ordering::Relaxed) {
                return;
            }
            if !mirror.writes() {
                waiting = false;
                continue;
            }
            // What fails stays in the journal, and is tried again soon.
            waiting = match mirror.send(&mut up, news.as_deref()) {
                Ok(()) => false,
                Err(e) => {
                    if unreachable(&e) {
                        up.reset();
                    }
                    true
                }
            };
        }
    }

    /// Follows what happens on the server and brings to the mirrors what
    /// changes in their lanes.
    fn follow(&self) {
        let mut up = Upstream::new(&self.remote);
        // `None`: not in step, everything is looked at before listening.
        let mut after: Option<u64> = None;
        loop {
            let mirrors = self.open();
            if mirrors.is_empty() {
                after = None;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let Some(cursor) = after else {
                // Where the events are now, then every file: nothing
                // falls between the two.
                let now = up.call("events", json!({})).ok().and_then(|now| now["next"].as_u64());
                let in_step = now.is_some() && mirrors.iter().all(|mirror| self.bring(&mut up, mirror, None));
                match in_step {
                    true => after = now,
                    false => std::thread::sleep(Duration::from_secs(1)),
                }
                continue;
            };
            let heard = up.call("events", json!({ "after": cursor, "wait_ms": EVENTS_WAIT_MS }));
            let Ok(heard) = heard else {
                after = None;
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };
            let events: Vec<rpc::Event> = serde_json::from_value(heard["events"].clone()).unwrap_or_default();
            let next = heard["next"].as_u64().unwrap_or(cursor);
            // Events that were not kept, or a server that started again.
            if next < cursor || events.first().is_some_and(|first| first.seq > cursor + 1) {
                after = None;
                continue;
            }
            after = Some(next);
            for event in &events {
                let Some(mirror) = event.lane.as_deref().and_then(|lane| self.mirror(lane)) else { continue };
                if mirror.closed() {
                    continue;
                }
                if event.kind == "lane_closed" {
                    let _ = mirror.set_closed();
                } else if CHANGES_FILES.contains(&event.kind.as_str()) {
                    let paths =
                        (event.kind == "saved" && !event.paths.is_empty()).then_some(event.paths.as_slice());
                    if !self.bring(&mut up, &mirror, paths) {
                        after = None;
                    }
                }
            }
        }
    }

    /// Brings a mirror in step. `false` when the server could not be asked.
    fn bring(&self, up: &mut Upstream, mirror: &Mirror, paths: Option<&[String]>) -> bool {
        match mirror.refresh(up, paths) {
            Ok(_) => true,
            Err(ClientError::Rpc(error)) if error.code == rpc::NO_SUCH_LANE => {
                let _ = mirror.set_closed();
                true
            }
            Err(e) => !unreachable(&e),
        }
    }

    fn forward(&self, up: &mut Upstream, method: &str, params: &Value) -> Answer {
        up.call(method, params.clone()).map_err(|e| match e {
            ClientError::Rpc(error) => (error.code, error.message),
            e => (rpc::INTERNAL_ERROR, format!("the server ({}): {e}", self.remote.place())),
        })
    }

    /// The working copies an answer names are the server's: here they are
    /// the mirrors, for the lanes that have one.
    fn here(&self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                if map.contains_key("workdir") {
                    let lane = map.get("id").or_else(|| map.get("lane")).and_then(Value::as_str);
                    let mirror = lane.and_then(|lane| self.mirror(lane));
                    map.insert("workdir".to_owned(), json!(mirror.map(|mirror| mirror.work.clone())));
                }
                map.values_mut().for_each(|value| self.here(value));
            }
            Value::Array(items) => items.iter_mut().for_each(|value| self.here(value)),
            _ => {}
        }
    }

    /// Makes the files of a lane available here: to follow it, or to write
    /// in it.
    fn attach(
        self: &Arc<Self>,
        up: &mut Upstream,
        spec: &str,
        write: bool,
    ) -> Result<MirrorInfo, (i64, String)> {
        // The server says which lane the name stands for.
        let named = self.forward(up, "lane_files", &json!({ "lane": spec, "paths": [] }))?;
        let lane =
            named["lane"].as_str().ok_or((rpc::INTERNAL_ERROR, "the server named no lane".to_owned()))?;
        if write {
            self.forward(up, "claim_writer", &json!({ "lane": lane, "writer": self.writer }))?;
        }
        let mirror = match self.mirror(lane) {
            Some(mirror) if !mirror.closed() => mirror,
            _ => {
                let lanes = self.forward(up, "lanes", &json!({}))?;
                let agent = lanes
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|info| info["id"] == lane)
                    .and_then(|info| info["agent"].as_str())
                    .map(str::to_owned);
                self.start(Mirror::create(&self.root, lane, agent, &self.writer).map_err(failed)?)
            }
        };
        mirror.set_write(write, None).map_err(failed)?;
        mirror.refresh(up, None).map_err(failed)?;
        // What was changed here while the copy only followed is a save now.
        mirror.touched_all();
        Ok(mirror.info())
    }

    fn detach(&self, up: &mut Upstream, spec: &str, discard: bool) -> Answer {
        let mirror = self.mirror(spec).ok_or((rpc::REFUSED, format!("{spec}: no copy of this lane here")))?;
        let waiting = match mirror.writes() {
            true => mirror.flush(up),
            false => mirror.waiting(),
        };
        if let Some(why) = waiting.refusal(&mirror.lane).filter(|_| !discard) {
            return Err((
                rpc::REFUSED,
                format!("{why} (`kito lane detach {} --discard` drops them)", mirror.lane),
            ));
        }
        self.drop_mirror(&mirror.lane).map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
        Ok(json!({ "lane": mirror.lane, "workdir": mirror.work, "removed": mirror.work != self.root }))
    }

    fn drop_mirror(&self, lane: &str) -> std::io::Result<()> {
        let running = self.mirrors().remove(lane);
        match running {
            Some(running) => running.mirror.remove(),
            None => Ok(()),
        }
    }

    fn status(&self) -> Value {
        let mirrors: Vec<MirrorInfo> = self.mirrors().values().map(|running| running.mirror.info()).collect();
        json!({
            "connected_to": self.remote.place(),
            "repo": self.remote.repo,
            "root": self.root,
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "writer": self.writer,
            "mirrors": mirrors,
        })
    }

    /// What a command changed where it should not have: the server says
    /// it of the lanes, this machine of its own main directory, which
    /// follows the main lane and keeps what is written in it to itself.
    fn after_shell(&self, up: &mut Upstream, params: &Value) -> Result<Value, (i64, String)> {
        let named =
            params.get("lanes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
        for mirror in named.filter_map(|lane| self.mirror(lane)).filter(|mirror| mirror.writes()) {
            mirror.flush(up);
        }
        let mut asked = params.clone();
        asked["main"] = json!(false);
        let mut answer = self.forward(up, "after_shell", &asked)?;
        let since = params.get("since_ms").and_then(Value::as_u64).unwrap_or(0);
        // The directory follows the main lane, unless the lane is written from here.
        let held: Vec<String> = match self.mirror("main").filter(|main| !main.writes()) {
            Some(main) => main.changed_here(since),
            None => Vec::new(),
        };
        if !held.is_empty() {
            let here = format!(
                "Kito: files of the main directory were changed: {}. This directory follows the main \
                 lane: the change stays on this machine and is recorded nowhere (`kito lane take main \
                 <file> --theirs` puts a file back as the main lane has it). To change them, open a \
                 lane with the open_lane tool and work in its working copy.",
                held.join(", ")
            );
            let before = answer.get("message").and_then(Value::as_str).unwrap_or_default();
            answer["message"] = json!(format!("{here}\n{before}").trim_end());
            answer["main"] = json!([{ "at_ms": since, "paths": held, "put_back": false }]);
        }
        Ok(answer)
    }

    /// Before a request that must find on the server everything saved
    /// here: sends it, and says no if it does not get there.
    fn barrier(&self, up: &mut Upstream, params: &Value, strict: bool) -> Result<(), (i64, String)> {
        let mirrors = match params.get("lane").and_then(Value::as_str) {
            Some(spec) => self.mirror(spec).into_iter().collect(),
            None => self.open(),
        };
        for mirror in mirrors.iter().filter(|mirror| mirror.writes()) {
            let waiting = mirror.flush(up);
            if let Some(why) = waiting.refusal(&mirror.lane).filter(|_| strict) {
                return Err((rpc::REFUSED, why));
            }
        }
        Ok(())
    }

    fn handle(self: &Arc<Self>, up: &mut Upstream, request: &Request) -> Answer {
        let params = &request.params;
        let text = |key: &str| {
            params
                .get(key)
                .and_then(Value::as_str)
                .ok_or((rpc::INVALID_PARAMS, format!("missing parameter `{key}`")))
        };
        let flag = |key: &str| params.get(key).and_then(Value::as_bool).unwrap_or(false);
        let mut answer = match request.method.as_str() {
            "mirror_status" => return Ok(self.status()),
            "ping" => return Ok(json!({ "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id() })),
            "shutdown" => return Ok(json!({})),
            "attach" => {
                let info = self.attach(up, text("lane")?, flag("write"))?;
                return serde_json::to_value(info).map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()));
            }
            "detach" => return self.detach(up, text("lane")?, flag("discard")),
            "take" => {
                let spec = text("lane")?;
                let mirror =
                    self.mirror(spec).ok_or((rpc::REFUSED, format!("{spec}: no copy of this lane here")))?;
                let done = mirror.take(up, text("path")?, flag("mine")).map_err(failed)?;
                return Ok(json!({ "lane": mirror.lane, "done": done }));
            }
            "open_lane" => {
                let opened = self.forward(up, "open_lane", params)?;
                let lane = opened["id"].as_str().unwrap_or_default().to_owned();
                // The lane is edited here: its files come first.
                self.attach(up, &lane, true).map_err(|(code, why)| {
                    let hint = format!(
                        "{lane} is open on the server, but its files did not get here: {why} \
                         (`kito lane attach {lane} --write` tries again)"
                    );
                    (code, hint)
                })?;
                opened
            }
            "close_lane" => {
                // What was saved here goes with the lane, if it can.
                self.barrier(up, params, false)?;
                let closed = self.forward(up, "close_lane", params)?;
                if let Some(lane) = closed["id"].as_str() {
                    let _ = self.drop_mirror(lane);
                }
                closed
            }
            method @ ("request_integration" | "sync" | "rename_symbol") => {
                self.barrier(up, params, true)?;
                let answer = self.forward(up, method, params)?;
                // A rename is made on the server, in the files of the lane.
                if method == "rename_symbol" {
                    if let Some(mirror) = self.mirror_asked(params) {
                        self.bring(up, &mirror, None);
                    }
                }
                answer
            }
            "integration_result" => {
                let answer = self.forward(up, "integration_result", params)?;
                // An integrated lane starts again from the main lane: its
                // files are here before the answer is.
                if let Some(mirror) = lane_in(&answer).and_then(|lane| self.mirror(lane)) {
                    self.bring(up, &mirror, None);
                }
                answer
            }
            method @ ("set_intent" | "lane_status" | "log" | "extend_scope" | "why_graph") => {
                self.barrier(up, params, false)?;
                self.forward(up, method, params)?
            }
            // Asked before an edit in a lane: the server judges the file
            // as it is here. The main directory has nothing to send.
            "before_edit" => {
                if params.get("lane").is_some() {
                    self.barrier(up, params, false)?;
                }
                self.forward(up, "before_edit", params)?
            }
            // Asked before an integration: of everything the lane has here.
            "reasons_behind" => {
                self.barrier(up, params, true)?;
                self.forward(up, "reasons_behind", params)?
            }
            "after_shell" => self.after_shell(up, params)?,
            method => self.forward(up, method, params)?,
        };
        self.here(&mut answer);
        Ok(answer)
    }
}

fn serve_client(daemon: &Arc<Daemon>, (read, mut writer): crate::ipc::Halves) {
    let kept = daemon.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
    let mut up = kept.unwrap_or_else(|| Upstream::new(&daemon.remote));
    // Whoever speaks first says the key of the socket.
    let mut trusted = daemon.key.is_none();
    for line in BufReader::new(read).lines() {
        let Ok(line) = line else { break };
        if !trusted {
            match daemon.key.as_deref().and_then(|key| rpc::key_said(&line, key)) {
                Some(true) => {
                    trusted = true;
                    continue;
                }
                _ => {
                    let id = serde_json::from_str::<Request>(&line).map_or(0, |request| request.id);
                    if let Ok(mut out) = serde_json::to_vec(&Response::err(id, rpc::REFUSED, rpc::NO_KEY)) {
                        out.push(b'\n');
                        let _ = writer.write_all(&out).and_then(|()| writer.flush());
                    }
                    return;
                }
            }
        }
        let (response, last) = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let response = match daemon.handle(&mut up, &request) {
                    Ok(value) => Response::ok(request.id, value),
                    Err((code, message)) => Response::err(request.id, code, message),
                };
                (response, request.method == "shutdown")
            }
            Err(e) => (Response::err(0, rpc::PARSE_ERROR, e.to_string()), false),
        };
        let Ok(mut out) = serde_json::to_vec(&response) else { break };
        out.push(b'\n');
        if writer.write_all(&out).and_then(|()| writer.flush()).is_err() {
            break;
        }
        if last {
            // Nothing to put away: what the mirrors know is on disk at
            // every step.
            let _ = std::fs::remove_file(rpc::socket_path(&daemon.root));
            std::process::exit(0);
        }
    }
    // The connection to the server serves the next client.
    let mut idle = daemon.idle.lock().unwrap_or_else(|e| e.into_inner());
    if up.connected() && idle.len() < IDLE_KEPT {
        idle.push(up);
    }
}

/// Serves a directory connected to a repository on a server, until told
/// to stop.
pub fn serve(root: &Path) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
    let remote =
        Remote::load(&root).ok_or_else(|| format!("{} is not connected to a server", root.display()))?;
    let kito = root.join(kito_store::STORE_DIR);

    // One daemon for a directory.
    let lock_path = kito.join("kitod.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("{}: {e}", lock_path.display()))?;
    if lock.try_lock().is_err() {
        return Err(format!("another daemon is serving {}", root.display()));
    }

    let daemon = Arc::new(Daemon {
        writer: writer_id(&kito),
        mirrors: Mutex::default(),
        idle: Mutex::default(),
        remote: remote.clone(),
        key: rpc::ensure_socket_key(&root).ok(),
        root: root.clone(),
    });
    for dir in std::fs::read_dir(kito.join("lanes")).into_iter().flatten().flatten() {
        match Mirror::load(&root, &dir.path(), &daemon.writer) {
            Some(mirror) if dir.file_name().to_string_lossy() == dir_name(&mirror.lane) => {
                daemon.start(mirror);
            }
            _ => eprintln!("kitod: {}: not a copy of a lane, left alone", dir.path().display()),
        }
    }

    let socket = rpc::socket_path(&root);
    let (_, clients) = crate::ipc::blocking(&root).map_err(|e| format!("{}: {e}", socket.display()))?;
    eprintln!(
        "kitod: listening on {} for {}, connected to `{}` on {}",
        socket.display(),
        root.display(),
        remote.repo,
        remote.place()
    );

    let following = daemon.clone();
    std::thread::spawn(move || following.follow());
    // A directory that is no longer connected needs no daemon.
    let connected = kito.join(crate::client::REMOTE_FILE);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        if !connected.is_file() {
            eprintln!("kitod: {} is gone: stopping", connected.display());
            std::process::exit(0);
        }
    });

    for client in clients {
        let Ok(client) = client else { continue };
        let daemon = daemon.clone();
        std::thread::spawn(move || serve_client(&daemon, client));
    }
    drop(lock);
    Ok(())
}
