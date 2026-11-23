//! A lane's working copy on a machine other than the repository's.
//!
//! The lane lives on the server; a mirror is its files kept in step on
//! another machine. Every mirror follows: what changes in the lane on the
//! server is written here, unless the file was changed here in the
//! meantime. A mirror that writes also sends: each save found here goes
//! into a journal on disk first, then to the server in order, and leaves
//! the journal when the server has answered. A daemon killed at any moment
//! finds the journal and the files where it left them, and carries on.
//!
//! What a mirror knows of the server is its manifest: for each file, the
//! content last known to be the same on both sides. A save is sent with
//! that content as its base, so the server can tell a lane that moved on.

use crate::client::{Client, ClientError, Remote};
use crate::files::{FileEntry, Put};
use crate::ignore::IgnoreRules;
use crate::rpc;
use kito_core::{BlobId, FileMode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Requests that may be sent twice without harm: they are sent again when
/// the connection is found dead.
const REPEATABLE: &[&str] = &[
    "lane_files",
    "get_blobs",
    "put_file",
    "remove_file",
    "claim_writer",
    "events",
    "status",
    "log",
    "why",
    "lanes",
    "lane_status",
    "board",
    "who_touches",
    "conflicts",
    "integration_result",
    "sync",
    "set_intent",
    "extend_scope",
    "why_graph",
    "reasons",
    "after_shell",
    "lane_diff",
    "comments",
    "review",
];

/// A connection left alone this long is checked before it is trusted with
/// a request that must not be sent twice.
const IDLE: Duration = Duration::from_secs(1);

/// How many contents are asked for at once.
const BLOBS_ASKED: usize = 200;

/// A file changed less than this long ago is read again even if its size
/// and time are the ones seen before.
const RECENT: Duration = Duration::from_secs(2);

/// Changes to the manifest are appended to a log; this many, and the
/// manifest is written whole again.
const LOG_LINES: usize = 2000;

/// The connection of a client daemon to its server.
pub struct Upstream {
    remote: Remote,
    /// The directory whose connection this is: read again before each new
    /// connection, so that a session renewed or a token replaced in the
    /// person's servers is the one used (ADR 0023).
    root: Option<PathBuf>,
    client: Option<Client>,
    used: Instant,
}

impl Upstream {
    pub fn new(remote: &Remote) -> Upstream {
        Upstream { remote: remote.clone(), root: None, client: None, used: Instant::now() }
    }

    /// The connection of the directory `root`, connected as `remote` for now.
    pub fn of(root: &Path, remote: &Remote) -> Upstream {
        Upstream { root: Some(root.to_owned()), ..Upstream::new(remote) }
    }

    fn open(&mut self) -> Result<Client, ClientError> {
        if let Some(now) = self.root.as_deref().and_then(Remote::load) {
            self.remote = now;
        }
        Client::remote(&self.remote, true)
    }

    /// Sends a request to the repository on the server.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let again = REPEATABLE.contains(&method);
        // A connection left idle may be gone without a word.
        if !again && self.used.elapsed() > IDLE {
            if let Some(client) = &mut self.client {
                if client.call_value("events", json!({})).is_err() {
                    self.client = None;
                }
            }
        }
        for attempt in 0..2 {
            let client = match &mut self.client {
                Some(client) => client,
                None => {
                    let client = self.open()?;
                    self.client.insert(client)
                }
            };
            match client.call_value(method, params.clone()) {
                Ok(value) => {
                    self.used = Instant::now();
                    return Ok(value);
                }
                Err(ClientError::Rpc(error)) => {
                    self.used = Instant::now();
                    return Err(ClientError::Rpc(error));
                }
                Err(e) => {
                    self.client = None;
                    if !again || attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        Err(ClientError::Server(format!("{}: no answer", self.remote.place())))
    }

    /// Forgets the connection: the next request opens a new one.
    pub fn reset(&mut self) {
        self.client = None;
    }

    /// `true` if a connection is open, as far as is known.
    pub fn connected(&self) -> bool {
        self.client.is_some()
    }
}

/// `true` when the server could not be asked at all, as opposed to having
/// answered no.
pub fn unreachable(error: &ClientError) -> bool {
    !matches!(error, ClientError::Rpc(_) | ClientError::Refused(_))
}

/// A file as both sides have it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Synced {
    pub blob: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub symlink: bool,
}

impl Synced {
    fn mode(&self) -> FileMode {
        match (self.symlink, self.executable) {
            (true, _) => FileMode::Symlink,
            (false, true) => FileMode::Executable,
            (false, false) => FileMode::Regular,
        }
    }
}

impl From<&FileEntry> for Synced {
    fn from(entry: &FileEntry) -> Synced {
        Synced { blob: entry.blob.clone(), executable: entry.executable, symlink: entry.symlink }
    }
}

/// A save the server would not take, so that it is not offered again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// The content that was refused; `None`: a removal.
    pub blob: Option<String>,
    pub reason: String,
}

/// What a mirror remembers between two runs of the daemon.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    lane: String,
    #[serde(default)]
    agent: Option<String>,
    /// Saves made here are sent to the lane.
    #[serde(default)]
    write: bool,
    /// The lane was closed on the server.
    #[serde(default)]
    closed: bool,
    /// Why it no longer writes, when it did.
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    files: BTreeMap<String, Synced>,
    /// Files changed here and on the server in ways that do not merge,
    /// with the content the server has (`None`: removed there).
    #[serde(default)]
    diverged: BTreeMap<String, Option<String>>,
    #[serde(default)]
    refused: BTreeMap<String, Refusal>,
}

/// A change to the manifest, as the log keeps it.
#[derive(Serialize, Deserialize)]
struct Line {
    path: String,
    /// `None`: the file is gone on both sides.
    file: Option<Synced>,
}

/// A save waiting to reach the server.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    seq: u64,
    path: String,
    /// The content saved; `None`: the file was removed.
    blob: Option<String>,
    #[serde(default)]
    executable: bool,
    /// The content the save started from: what the server is expected to have.
    base: Option<String>,
}

#[derive(Default)]
struct State {
    saved: Saved,
    journal: VecDeque<Entry>,
    next_seq: u64,
    log_lines: usize,
    /// Size, time and content of the files as last read.
    stats: HashMap<String, (u64, SystemTime, Synced)>,
    /// Files changed here that are not sent: this copy only follows.
    held: BTreeSet<String>,
    /// Why the last attempt to reach the server failed.
    problem: Option<String>,
    synced_ms: u64,
}

/// What the files of a mirror are waiting for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    /// Saves not yet on the server.
    pub pending: usize,
    /// Files that differ here and on the server and do not merge.
    pub diverged: Vec<String>,
    /// Why saves are not getting through, if they are not.
    pub problem: Option<String>,
}

impl Waiting {
    /// In words, for who asked for everything to be on the server.
    pub fn refusal(&self, lane: &str) -> Option<String> {
        if !self.diverged.is_empty() {
            let path = self.diverged.first().map(String::as_str).unwrap_or_default();
            return Some(format!(
                "{} changed here and on the server in ways that do not merge: keep one with \
                 `kito lane take {lane} {path} --mine` (this machine's) or `--theirs` (the server's)",
                self.diverged.join(", ")
            ));
        }
        if self.pending > 0 {
            let why = self.problem.clone().unwrap_or_else(|| "the server did not take them".to_owned());
            let saves =
                if self.pending == 1 { "1 save".to_owned() } else { format!("{} saves", self.pending) };
            return Some(format!("{saves} made here did not reach the server yet: {why}"));
        }
        None
    }
}

/// A mirror, as `kito daemon status` shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorInfo {
    pub lane: String,
    pub agent: Option<String>,
    pub workdir: PathBuf,
    pub write: bool,
    pub closed: bool,
    pub note: Option<String>,
    pub files: usize,
    pub pending: usize,
    pub diverged: Vec<String>,
    pub refused: BTreeMap<String, String>,
    pub held: Vec<String>,
    pub problem: Option<String>,
    /// When it was last brought in step with the server, in milliseconds
    /// since the Unix epoch.
    pub synced_ms: u64,
}

/// Paths the watcher saw change, each with the moment it is looked at.
#[derive(Default)]
struct News {
    due: BTreeMap<PathBuf, Instant>,
    /// Look at everything.
    all: bool,
}

pub struct Mirror {
    pub lane: String,
    dir: PathBuf,
    pub work: PathBuf,
    /// This machine, as the server knows the copy that writes in a lane.
    writer: String,
    /// One at a time: looking for saves, sending them, taking the server's.
    busy: Mutex<()>,
    state: Mutex<State>,
    ignore: IgnoreRules,
    news: Mutex<News>,
    woken: Condvar,
    pub(crate) stop: AtomicBool,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64)
}

fn other(text: impl Into<String>) -> ClientError {
    ClientError::Protocol(text.into())
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ClientError + '_ {
    move |e| ClientError::Io(std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// Writes a file so that it is either the old one or the new one, also
/// after a crash.
fn write_whole(path: &Path, bytes: &[u8]) -> Result<(), ClientError> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).map_err(io(&tmp))?;
    file.write_all(bytes).map_err(io(&tmp))?;
    file.sync_data().map_err(io(&tmp))?;
    fs::rename(&tmp, path).map_err(io(path))
}

/// `true` for the main lane, however it is written.
pub fn is_main(lane: &str) -> bool {
    lane.parse::<kito_core::LaneId>().is_ok_and(|lane| lane.is_main())
}

/// The directory of a lane's mirror under `.kito/lanes`: `3`, `main`.
pub fn dir_name(lane: &str) -> String {
    lane.strip_prefix("lane/").unwrap_or(lane).to_owned()
}

/// A path the server names, as a path inside a working copy; `None` if it
/// would lead anywhere else.
fn inside(path: &str) -> Option<PathBuf> {
    let rel = Path::new(path);
    let mut parts = rel.components();
    let first = parts.next()?;
    let normal = |part: Component| matches!(part, Component::Normal(_));
    let reserved = matches!(first, Component::Normal(name) if name == ".kito" || name == ".git");
    (normal(first) && !reserved && parts.all(normal)).then(|| rel.to_owned())
}

/// What is at a path of the working copy.
enum Local {
    Gone,
    File(Synced, Vec<u8>),
}

impl Local {
    fn synced(&self) -> Option<&Synced> {
        match self {
            Local::Gone => None,
            Local::File(file, _) => Some(file),
        }
    }
}

impl Mirror {
    fn at(root: &Path, lane: &str, writer: &str, saved: Saved) -> Mirror {
        let dir = root.join(kito_store::STORE_DIR).join("lanes").join(dir_name(lane));
        // The main lane's files are the connected directory itself, as in
        // a repository; a lane's are a working copy of its own.
        let work = if is_main(lane) { root.to_owned() } else { dir.join("work") };
        Mirror {
            lane: lane.to_owned(),
            ignore: IgnoreRules::new(&work, &[]),
            work,
            dir,
            writer: writer.to_owned(),
            busy: Mutex::new(()),
            state: Mutex::new(State { saved, next_seq: 1, ..State::default() }),
            news: Mutex::new(News::default()),
            woken: Condvar::new(),
            stop: AtomicBool::new(false),
        }
    }

    /// A new mirror of a lane, with nothing in it yet.
    pub fn create(
        root: &Path,
        lane: &str,
        agent: Option<String>,
        writer: &str,
    ) -> Result<Mirror, ClientError> {
        let saved = Saved { lane: lane.to_owned(), agent, ..Saved::default() };
        let mirror = Mirror::at(root, lane, writer, saved);
        fs::create_dir_all(&mirror.work).map_err(io(&mirror.work))?;
        fs::create_dir_all(mirror.dir.join("journal")).map_err(io(&mirror.dir))?;
        mirror.save(&mut mirror.state())?;
        Ok(mirror)
    }

    /// The mirror a daemon left in `dir`, with the saves it had not sent.
    pub fn load(root: &Path, dir: &Path, writer: &str) -> Option<Mirror> {
        let text = fs::read_to_string(dir.join("mirror.json")).ok()?;
        let mut saved: Saved = serde_json::from_str(&text).ok()?;
        // What changed since the manifest was last written whole. A last
        // line cut short by a crash says nothing.
        let mut log_lines = 0;
        for line in fs::read_to_string(dir.join("mirror.log")).unwrap_or_default().lines() {
            let Ok(line) = serde_json::from_str::<Line>(line) else { break };
            log_lines += 1;
            match line.file {
                Some(file) => saved.files.insert(line.path, file),
                None => saved.files.remove(&line.path),
            };
        }
        // The saves that were waiting. What a crash left half-made (a
        // content with no entry, an entry cut short) is no save: if the
        // file still differs, the next look finds it again.
        let mut journal: Vec<Entry> = Vec::new();
        let files: Vec<PathBuf> = fs::read_dir(dir.join("journal"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|file| file.path())
            .collect();
        for path in files.iter().filter(|path| path.extension().is_some_and(|e| e == "json")) {
            let entry: Option<Entry> =
                fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok());
            journal
                .extend(entry.filter(|entry| entry.blob.is_none() || path.with_extension("data").is_file()));
        }
        let kept: BTreeSet<String> = journal.iter().map(|entry| format!("{:020}", entry.seq)).collect();
        for path in &files {
            let stem = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
            if !kept.contains(&stem) || path.extension().is_some_and(|e| e == "tmp") {
                let _ = fs::remove_file(path);
            }
        }
        journal.sort_by_key(|entry| entry.seq);
        let lane = saved.lane.clone();
        let mirror = Mirror::at(root, &lane, writer, saved);
        {
            let mut state = mirror.state();
            state.next_seq = journal.last().map_or(1, |entry| entry.seq + 1);
            state.journal = journal.into();
            state.log_lines = log_lines;
        }
        fs::create_dir_all(&mirror.work).ok()?;
        fs::create_dir_all(mirror.dir.join("journal")).ok()?;
        Some(mirror)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn busy(&self) -> std::sync::MutexGuard<'_, ()> {
        self.busy.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes the manifest whole.
    fn save(&self, state: &mut State) -> Result<(), ClientError> {
        let text = serde_json::to_vec(&state.saved).map_err(|e| other(e.to_string()))?;
        write_whole(&self.dir.join("mirror.json"), &text)?;
        // What the log said is in the manifest now.
        let log = self.dir.join("mirror.log");
        fs::write(&log, b"").map_err(io(&log))?;
        state.log_lines = 0;
        Ok(())
    }

    /// One file is the same on both sides now (`None`: gone on both).
    fn set(&self, state: &mut State, path: &str, file: Option<Synced>) -> Result<(), ClientError> {
        match &file {
            Some(file) => state.saved.files.insert(path.to_owned(), file.clone()),
            None => state.saved.files.remove(path),
        };
        if state.log_lines >= LOG_LINES {
            return self.save(state);
        }
        let mut line =
            serde_json::to_vec(&Line { path: path.to_owned(), file }).map_err(|e| other(e.to_string()))?;
        line.push(b'\n');
        let path = self.dir.join("mirror.log");
        let mut log = fs::OpenOptions::new().create(true).append(true).open(&path).map_err(io(&path))?;
        log.write_all(&line).map_err(io(&path))?;
        log.sync_data().map_err(io(&path))?;
        state.log_lines += 1;
        Ok(())
    }

    pub fn writes(&self) -> bool {
        let state = self.state();
        state.saved.write && !state.saved.closed
    }

    pub fn agent(&self) -> Option<String> {
        self.state().saved.agent.clone()
    }

    pub fn closed(&self) -> bool {
        self.state().saved.closed
    }

    /// Makes the mirror one that sends its saves, or one that only follows.
    pub fn set_write(&self, write: bool, note: Option<String>) -> Result<(), ClientError> {
        let mut state = self.state();
        state.saved.write = write;
        state.saved.note = note;
        if write {
            state.held.clear();
        }
        self.save(&mut state)
    }

    /// The lane was closed on the server: nothing more comes or goes.
    pub fn set_closed(&self) -> Result<(), ClientError> {
        let mut state = self.state();
        state.saved.closed = true;
        state.saved.note = Some("the lane was closed".to_owned());
        self.save(&mut state)
    }

    pub fn info(&self) -> MirrorInfo {
        let state = self.state();
        MirrorInfo {
            lane: self.lane.clone(),
            agent: state.saved.agent.clone(),
            workdir: self.work.clone(),
            write: state.saved.write,
            closed: state.saved.closed,
            note: state.saved.note.clone(),
            files: state.saved.files.len(),
            pending: state.journal.len(),
            diverged: state.saved.diverged.keys().cloned().collect(),
            refused: state.saved.refused.iter().map(|(path, r)| (path.clone(), r.reason.clone())).collect(),
            held: state.held.iter().cloned().collect(),
            problem: state.problem.clone(),
            synced_ms: state.synced_ms,
        }
    }

    pub fn waiting(&self) -> Waiting {
        let state = self.state();
        Waiting {
            pending: state.journal.len(),
            diverged: state.saved.diverged.keys().cloned().collect(),
            problem: state.problem.clone(),
        }
    }

    /// What is at a path here. Files whose size and time are the ones seen
    /// before are not read again, unless they changed a moment ago.
    fn local(&self, state: &mut State, path: &str, content: bool) -> Local {
        let abs = self.work.join(path);
        let Ok(meta) = fs::symlink_metadata(&abs) else {
            state.stats.remove(path);
            return Local::Gone;
        };
        if meta.file_type().is_symlink() {
            let Ok(target) = fs::read_link(&abs) else { return Local::Gone };
            #[cfg(unix)]
            let bytes = std::os::unix::ffi::OsStrExt::as_bytes(target.as_os_str()).to_vec();
            #[cfg(not(unix))]
            let bytes = target.to_string_lossy().replace('\\', "/").into_bytes();
            let file = Synced { blob: BlobId::of(&bytes).to_string(), executable: false, symlink: true };
            return Local::File(file, bytes);
        }
        if !meta.is_file() {
            state.stats.remove(path);
            return Local::Gone;
        }
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let settled = modified.elapsed().is_ok_and(|age| age > RECENT);
        if !content && settled {
            if let Some((len, time, file)) = state.stats.get(path) {
                if *len == meta.len() && *time == modified {
                    return Local::File(file.clone(), Vec::new());
                }
            }
        }
        let Ok(bytes) = fs::read(&abs) else {
            state.stats.remove(path);
            return Local::Gone;
        };
        // Windows has no executable bit: what was known of the file stays (ADR 0002).
        #[cfg(unix)]
        let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let executable = state.stats.get(path).is_some_and(|(_, _, known)| known.executable);
        let file = Synced { blob: BlobId::of(&bytes).to_string(), executable, symlink: false };
        state.stats.insert(path.to_owned(), (meta.len(), modified, file.clone()));
        Local::File(file, bytes)
    }

    /// The files of the lane written here since `since_ms` that are not as
    /// they were last in step with the server: for a copy that only
    /// follows, what was changed in it by hand.
    pub fn changed_here(&self, since_ms: u64) -> Vec<String> {
        let mut state = self.state();
        let known: Vec<(String, Synced)> =
            state.saved.files.iter().map(|(path, file)| (path.clone(), file.clone())).collect();
        let since = SystemTime::UNIX_EPOCH + Duration::from_millis(since_ms);
        let mut changed = Vec::new();
        for (path, file) in known {
            let written = fs::symlink_metadata(self.work.join(&path)).and_then(|meta| meta.modified());
            if written.is_ok_and(|at| at >= since)
                && self.local(&mut state, &path, false).synced() != Some(&file)
            {
                changed.push(path);
            }
        }
        changed
    }

    /// `true` if nothing between the working copy and the file is a
    /// symbolic link: a file is never written through one.
    fn reachable(&self, rel: &Path) -> bool {
        let mut at = self.work.clone();
        let Some(parent) = rel.parent() else { return true };
        for part in parent.components() {
            at.push(part);
            if fs::symlink_metadata(&at).is_ok_and(|meta| meta.file_type().is_symlink()) {
                return false;
            }
        }
        true
    }

    fn pending_for(state: &State, path: &str) -> Option<Option<Synced>> {
        state.journal.iter().rev().find(|entry| entry.path == path).map(|entry| {
            entry.blob.as_ref().map(|blob| Synced {
                blob: blob.clone(),
                executable: entry.executable,
                symlink: false,
            })
        })
    }

    // ---- what the server has, brought here -------------------------------

    /// Brings the files in step with the lane on the server: all of them,
    /// or the ones named. A file changed here since it was last in step is
    /// left as it is. Returns how many files were written or removed.
    pub fn refresh(&self, up: &mut Upstream, paths: Option<&[String]>) -> Result<usize, ClientError> {
        let _busy = self.busy();
        self.refresh_held(up, paths)
    }

    fn refresh_held(&self, up: &mut Upstream, paths: Option<&[String]>) -> Result<usize, ClientError> {
        let mut asked = json!({ "lane": self.lane });
        if let Some(paths) = paths {
            asked["paths"] = json!(paths);
        }
        let listed = up.call("lane_files", asked)?;
        let files: Vec<FileEntry> =
            serde_json::from_value(listed["files"].clone()).map_err(|e| other(e.to_string()))?;
        let theirs: BTreeMap<&str, Synced> =
            files.iter().map(|file| (file.path.as_str(), file.into())).collect();

        // What differs from what was last the same on both sides.
        let (mut fetch, mut remove): (Vec<(String, Synced)>, Vec<String>) = Default::default();
        {
            let state = self.state();
            let known = &state.saved.files;
            let considered: BTreeSet<&str> = match paths {
                Some(paths) => paths.iter().map(String::as_str).collect(),
                None => known.keys().map(String::as_str).chain(theirs.keys().copied()).collect(),
            };
            for path in considered {
                match (theirs.get(path), known.get(path)) {
                    (Some(there), here) if here != Some(there) => {
                        fetch.push((path.to_owned(), there.clone()))
                    }
                    (None, Some(_)) => remove.push(path.to_owned()),
                    _ => {}
                }
            }
        }
        let bulk = fetch.len() + remove.len() > 16;
        let mut changed = 0;
        for part in fetch.chunks(BLOBS_ASKED) {
            let contents = self.contents(up, part)?;
            let mut state = self.state();
            for (path, there) in part {
                let Some(rel) = inside(path).filter(|rel| self.reachable(rel)) else { continue };
                let here = self.local(&mut state, path, true);
                let known = state.saved.files.get(path).cloned();
                if here.synced() != Some(there) {
                    // Changed here since: the save that is on its way settles it.
                    let untouched =
                        here.synced() == known.as_ref() && Self::pending_for(&state, path).is_none();
                    if !untouched {
                        if !state.saved.write {
                            state.held.insert(path.clone());
                        }
                        continue;
                    }
                    let Some(content) = contents.get(&there.blob) else { continue };
                    crate::repo::write_file(&self.work, &rel, there.mode(), content)
                        .map_err(|e| other(e.to_string()))?;
                    state.stats.remove(path);
                    changed += 1;
                }
                state.held.remove(path);
                match bulk {
                    true => drop(state.saved.files.insert(path.clone(), there.clone())),
                    false => self.set(&mut state, path, Some(there.clone()))?,
                }
            }
        }
        let mut state = self.state();
        for path in &remove {
            let Some(rel) = inside(path).filter(|rel| self.reachable(rel)) else { continue };
            let here = self.local(&mut state, path, true);
            let known = state.saved.files.get(path).cloned();
            if here.synced().is_some() {
                let untouched = here.synced() == known.as_ref() && Self::pending_for(&state, path).is_none();
                if !untouched {
                    if !state.saved.write {
                        state.held.insert(path.clone());
                    }
                    continue;
                }
                crate::integrate::remove_in(&self.work, &rel).map_err(|e| other(e.to_string()))?;
                changed += 1;
            }
            state.held.remove(path);
            match bulk {
                true => drop(state.saved.files.remove(path)),
                false => self.set(&mut state, path, None)?,
            }
        }
        if bulk {
            self.save(&mut state)?;
        }
        if changed > 0 {
            self.ignore.invalidate();
        }
        state.synced_ms = now_ms();
        state.problem = None;
        Ok(changed)
    }

    /// The contents of some files, asked from the server as many at a time
    /// as it gives.
    fn contents(
        &self,
        up: &mut Upstream,
        files: &[(String, Synced)],
    ) -> Result<HashMap<String, Vec<u8>>, ClientError> {
        let mut wanted: BTreeSet<&str> = files.iter().map(|(_, file)| file.blob.as_str()).collect();
        let mut contents = HashMap::new();
        while !wanted.is_empty() {
            let asked: Vec<&str> = wanted.iter().copied().collect();
            let answer = up.call("get_blobs", json!({ "blobs": asked }))?;
            let given = answer["blobs"].as_object().ok_or_else(|| other("get_blobs: no contents"))?;
            if given.is_empty() {
                return Err(other("get_blobs: the server gave nothing"));
            }
            for (blob, content) in given {
                let bytes =
                    content.as_str().and_then(rpc::decode).ok_or_else(|| other("get_blobs: not base64"))?;
                // A content is what its ID says, whoever sent it.
                if BlobId::of(&bytes).to_string() != *blob {
                    return Err(other(format!("get_blobs: {blob} is not the content received")));
                }
                wanted.remove(blob.as_str());
                contents.insert(blob.clone(), bytes);
            }
        }
        Ok(contents)
    }

    // ---- what is saved here, sent there ----------------------------------

    /// The watcher saw a path change: it is looked at after `wait`, or
    /// later if it changes again before.
    pub fn touched(&self, rel: PathBuf, wait: Duration) {
        let mut news = self.news.lock().unwrap_or_else(|e| e.into_inner());
        news.due.insert(rel, Instant::now() + wait);
        self.woken.notify_all();
    }

    /// Something may have changed anywhere in the working copy.
    pub fn touched_all(&self) {
        self.news.lock().unwrap_or_else(|e| e.into_inner()).all = true;
        self.woken.notify_all();
    }

    /// Waits for the watcher to have seen something, at most `limit`.
    /// Returns the paths to look at; `None`: all of them.
    pub fn wait_for_news(&self, limit: Duration) -> Option<Vec<String>> {
        let deadline = Instant::now() + limit;
        let mut news = self.news.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Some(Vec::new());
            }
            let now = Instant::now();
            if news.all {
                *news = News::default();
                return None;
            }
            let first = news.due.values().min().copied();
            match first {
                Some(due) if due <= now => {
                    let ready: Vec<PathBuf> = news
                        .due
                        .iter()
                        .filter(|(_, due)| **due <= now)
                        .map(|(path, _)| path.clone())
                        .collect();
                    for path in &ready {
                        news.due.remove(path);
                    }
                    return Some(ready.iter().map(|path| path.display().to_string()).collect());
                }
                _ if now >= deadline => return None,
                _ => {
                    let until = first.map_or(deadline, |due| due.min(deadline));
                    let wait = until.saturating_duration_since(now).max(Duration::from_millis(1));
                    news = self.woken.wait_timeout(news, wait).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
    }

    pub fn wake(&self) {
        self.woken.notify_all();
    }

    /// `true` for what a working copy holds that is no file of the lane.
    fn skipped(&self, rel: &Path, is_dir: bool) -> bool {
        let name = rel.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
        name.ends_with(".kito-tmp") || self.ignore.is_ignored(rel, is_dir)
    }

    fn walk(&self, dir: &Path, found: &mut BTreeSet<String>) {
        for entry in fs::read_dir(self.work.join(dir)).into_iter().flatten().flatten() {
            let rel = kito_core::paths::join_portable(dir, &entry.file_name());
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                let reserved = dir.as_os_str().is_empty()
                    && (entry.file_name() == ".kito" || entry.file_name() == ".git");
                if !reserved && !self.skipped(&rel, true) {
                    self.walk(&rel, found);
                }
            } else if !self.skipped(&rel, false) {
                found.insert(rel.display().to_string());
            }
        }
    }

    /// Looks for saves made here and puts each in the journal. Returns how
    /// many were found.
    fn scan(&self, paths: Option<&[String]>) -> Result<usize, ClientError> {
        let mut state = self.state();
        if !state.saved.write || state.saved.closed {
            return Ok(0);
        }
        let candidates: BTreeSet<String> = match paths {
            Some(paths) => {
                if paths.iter().any(|path| path.ends_with(".gitignore")) {
                    self.ignore.invalidate();
                }
                // A directory that appeared or went: what is under it.
                let mut found = BTreeSet::new();
                for path in paths {
                    let rel = Path::new(path);
                    if inside(path).is_none() {
                        continue;
                    }
                    if self.work.join(rel).is_dir() {
                        if !self.skipped(rel, true) {
                            self.walk(rel, &mut found);
                        }
                        continue;
                    }
                    let under = format!("{path}/");
                    found.extend(state.saved.files.keys().filter(|known| known.starts_with(&under)).cloned());
                    if state.saved.files.contains_key(path) || !self.skipped(rel, false) {
                        found.insert(path.clone());
                    }
                }
                found
            }
            None => {
                self.ignore.invalidate();
                let mut found: BTreeSet<String> = state.saved.files.keys().cloned().collect();
                found.extend(state.journal.iter().map(|entry| entry.path.clone()));
                self.walk(Path::new(""), &mut found);
                found
            }
        };
        let mut saves = 0;
        for path in candidates {
            let here = self.local(&mut state, &path, false);
            let expected = match Self::pending_for(&state, &path) {
                Some(pending) => pending,
                None => state.saved.files.get(&path).cloned(),
            };
            if here.synced() == expected.as_ref() {
                continue;
            }
            let refused = state.saved.refused.get(&path);
            if refused.is_some_and(|refused| refused.blob.as_ref() == here.synced().map(|file| &file.blob)) {
                continue;
            }
            // Read now, whole: the journal keeps what was saved, not what
            // the file will be by the time it is sent.
            let here = match here {
                Local::File(file, bytes) if bytes.is_empty() => match self.local(&mut state, &path, true) {
                    Local::File(again, bytes) if again == file => Local::File(again, bytes),
                    other => other,
                },
                other => other,
            };
            if let Local::File(file, _) = &here {
                if file.symlink {
                    let reason = "symbolic links are not sent from another machine".to_owned();
                    state
                        .saved
                        .refused
                        .insert(path.clone(), Refusal { blob: Some(file.blob.clone()), reason });
                    self.save(&mut state)?;
                    continue;
                }
            }
            let entry = Entry {
                seq: state.next_seq,
                path: path.clone(),
                blob: here.synced().map(|file| file.blob.clone()),
                executable: here.synced().is_some_and(|file| file.executable),
                base: expected.map(|file| file.blob),
            };
            let name = self.dir.join("journal").join(format!("{:020}", entry.seq));
            if let Local::File(_, bytes) = &here {
                write_whole(&name.with_extension("data"), bytes)?;
            }
            let text = serde_json::to_vec(&entry).map_err(|e| other(e.to_string()))?;
            write_whole(&name.with_extension("json"), &text)?;
            state.next_seq += 1;
            state.journal.push_back(entry);
            saves += 1;
        }
        Ok(saves)
    }

    fn forget(&self, entry: &Entry) {
        let name = self.dir.join("journal").join(format!("{:020}", entry.seq));
        let _ = fs::remove_file(name.with_extension("json"));
        let _ = fs::remove_file(name.with_extension("data"));
    }

    /// Sends the saves in the journal, oldest first, until it is empty or
    /// the server cannot be reached.
    fn drain(&self, up: &mut Upstream) -> Result<(), ClientError> {
        let mut merged: Vec<String> = Vec::new();
        let result = loop {
            let entry = {
                let state = self.state();
                if !state.saved.write || state.saved.closed {
                    break Ok(());
                }
                match state.journal.front() {
                    Some(entry) => entry.clone(),
                    None => break Ok(()),
                }
            };
            let name = self.dir.join("journal").join(format!("{:020}", entry.seq));
            let mut params =
                json!({ "lane": self.lane, "path": entry.path, "base": entry.base, "writer": self.writer });
            let method = match &entry.blob {
                Some(_) => {
                    // An entry whose content is gone says nothing: if the
                    // file still differs, the next look finds it again.
                    let Ok(content) = fs::read(name.with_extension("data")) else {
                        self.forget(&entry);
                        self.state().journal.pop_front();
                        continue;
                    };
                    params["content"] = json!(rpc::encode(&content));
                    params["executable"] = json!(entry.executable);
                    "put_file"
                }
                None => "remove_file",
            };
            let answer = up.call(method, params);
            let mut state = self.state();
            match answer {
                Ok(answer) => {
                    let put: Put = serde_json::from_value(answer).map_err(|e| other(e.to_string()))?;
                    let sent = entry.blob.as_ref().map(|blob| Synced {
                        blob: blob.clone(),
                        executable: entry.executable,
                        symlink: false,
                    });
                    match put.result.as_str() {
                        "conflict" => {
                            state.saved.diverged.insert(entry.path.clone(), put.blob);
                            self.save(&mut state)?;
                        }
                        result => {
                            // Merged: the lane has more than was sent, and
                            // it is fetched once the journal is empty.
                            if result == "merged" {
                                merged.push(entry.path.clone());
                            }
                            let settled = state.saved.diverged.remove(&entry.path).is_some()
                                | state.saved.refused.remove(&entry.path).is_some();
                            self.set(&mut state, &entry.path, sent)?;
                            if settled {
                                self.save(&mut state)?;
                            }
                        }
                    }
                    state.problem = None;
                    state.synced_ms = now_ms();
                }
                Err(ClientError::Rpc(error)) if error.code == rpc::REFUSED => {
                    let refusal = Refusal { blob: entry.blob.clone(), reason: error.message };
                    state.saved.refused.insert(entry.path.clone(), refusal);
                    self.save(&mut state)?;
                }
                Err(ClientError::Rpc(error)) if error.code == rpc::NOT_THE_WRITER => {
                    state.saved.write = false;
                    state.saved.note = Some(error.message.clone());
                    self.save(&mut state)?;
                    break Err(ClientError::Rpc(error));
                }
                Err(e) => {
                    state.problem = Some(e.to_string());
                    break Err(e);
                }
            }
            self.forget(&entry);
            state.journal.pop_front();
        };
        if !merged.is_empty() {
            self.refresh_held(up, Some(&merged))?;
        }
        result
    }

    /// Looks at the paths the watcher named (`None`: everywhere) and sends
    /// what was saved.
    pub fn send(&self, up: &mut Upstream, paths: Option<&[String]>) -> Result<(), ClientError> {
        let _busy = self.busy();
        self.scan(paths)?;
        self.drain(up)
    }

    /// The barrier: everything saved here so far is on the server, or the
    /// answer says what is not.
    pub fn flush(&self, up: &mut Upstream) -> Waiting {
        if self.writes() {
            let _busy = self.busy();
            let sent = self.scan(None).and_then(|_| self.drain(up));
            if let Err(e) = sent {
                self.state().problem = Some(e.to_string());
            }
        }
        self.waiting()
    }

    /// Settles a file that differs here and on the server: `mine` keeps
    /// this machine's and sends it over the server's, otherwise the
    /// server's replaces this machine's.
    pub fn take(&self, up: &mut Upstream, path: &str, mine: bool) -> Result<String, ClientError> {
        if mine && !self.writes() {
            return Err(other(format!(
                "this copy of {} only follows: `kito lane attach {} --write` first",
                self.lane, self.lane
            )));
        }
        let _busy = self.busy();
        let rel = inside(path).ok_or_else(|| other(format!("{path}: not a path inside the lane")))?;
        let listed = up.call("lane_files", json!({ "lane": self.lane, "paths": [path] }))?;
        let files: Vec<FileEntry> =
            serde_json::from_value(listed["files"].clone()).map_err(|e| other(e.to_string()))?;
        let there: Option<Synced> = files.first().map(Synced::from);
        {
            let mut state = self.state();
            // The saves of this file still waiting are overtaken either way.
            let overtaken: Vec<Entry> =
                state.journal.iter().filter(|entry| entry.path == path).cloned().collect();
            state.journal.retain(|entry| entry.path != path);
            overtaken.iter().for_each(|entry| self.forget(entry));
            state.saved.diverged.remove(path);
            state.saved.refused.remove(path);
            state.held.remove(path);
            if mine {
                // As if the save had started from what the server has now.
                match &there {
                    Some(there) => state.saved.files.insert(path.to_owned(), there.clone()),
                    None => state.saved.files.remove(path),
                };
            } else {
                match &there {
                    Some(there) => {
                        drop(state);
                        let mut contents = self.contents(up, &[(path.to_owned(), there.clone())])?;
                        let content = contents.remove(&there.blob).unwrap_or_default();
                        if !self.reachable(&rel) {
                            return Err(other(format!("{path}: behind a symbolic link here")));
                        }
                        crate::repo::write_file(&self.work, &rel, there.mode(), &content)
                            .map_err(|e| other(e.to_string()))?;
                        state = self.state();
                        state.saved.files.insert(path.to_owned(), there.clone());
                    }
                    None => {
                        crate::integrate::remove_in(&self.work, &rel).map_err(|e| other(e.to_string()))?;
                        state.saved.files.remove(path);
                    }
                }
                state.stats.remove(path);
            }
            self.save(&mut state)?;
        }
        if mine {
            self.scan(Some(&[path.to_owned()]))?;
            self.drain(up)?;
            return Ok(format!("{path}: this machine's is in the lane now"));
        }
        Ok(format!("{path}: the server's is here now"))
    }

    /// Removes the mirror from this machine. The main lane's files are the
    /// directory itself: they stay, and are no longer kept in step.
    pub fn remove(&self) -> std::io::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.wake();
        let _busy = self.busy();
        match fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}
