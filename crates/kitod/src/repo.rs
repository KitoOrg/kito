//! The capture engine: from the working directory to the ops in the log.
//!
//! It is synchronous code and knows nothing about watchers and sockets: the
//! daemon drives it, and the tests use it directly.

use crate::config::{Config, CONFIG_FILE, DEFAULT_CONFIG};
use crate::error::{Error, Result};
use crate::identity::{self, Identity};
use crate::ignore::IgnoreRules;
use crate::semantic::{self, Why};
use kito_core::{
    codec, Author, BlobId, Checkpoint, CheckpointId, FileMode, Hlc, HlcClock, Intent, IntentId, LaneId, Op,
    OpId, OpKind, SymbolId, SymbolTable, Tree, TreeEntry,
};
use kito_diff::ast::Identities;
use kito_diff::fileops::{ops_for_changes, Snapshot};
use kito_diff::text;
use kito_store::fs::atomic_write;
use kito_store::{Store, STORE_DIR};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A file modified less than this long ago does not enter the stat cache: a
/// second write in the same instant might not change its size and
/// modification time, and would escape the scan.
const RACY_WINDOW: Duration = Duration::from_millis(100);

/// A file modified less than this long ago may be in the middle of being
/// written: most programs save by emptying the file and then writing it,
/// and a capture that falls in between would record a file cut short (and,
/// if what is there happens to parse, the removal of every symbol after
/// the cut).
pub const SETTLE: Duration = Duration::from_millis(25);

/// Files that programs have written to and not yet closed, as the file
/// events tell (where the system reports when a file is closed: Linux).
///
/// A program that saves by emptying a file and then writing it can be
/// stopped in between for any length of time; no waiting is long enough to
/// be sure. Until the file is closed, it is not captured.
#[derive(Default)]
pub struct Writing {
    open: std::sync::Mutex<HashMap<PathBuf, Instant>>,
}

impl Writing {
    /// A program that keeps a file open and writes to it now and then (a
    /// log) is not waited for longer than this after its last write.
    const PATIENCE: Duration = Duration::from_secs(2);

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Instant>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The file at this absolute path was written to.
    pub fn modified(&self, path: &Path) {
        let mut open = self.lock();
        if open.len() > 4096 {
            open.retain(|_, at| at.elapsed() < Self::PATIENCE);
        }
        open.insert(path.to_owned(), Instant::now());
    }

    /// A program that had the file open for writing closed it.
    pub fn closed(&self, path: &Path) {
        self.lock().remove(path);
    }

    /// `true` if the file was written to and has not been closed since.
    pub fn is_open(&self, path: &Path) -> bool {
        let mut open = self.lock();
        match open.get(path).map(|at| at.elapsed() < Self::PATIENCE) {
            Some(true) => true,
            Some(false) => {
                open.remove(path);
                false
            }
            None => false,
        }
    }
}

/// What a capture finds at a path.
enum OnDisk {
    File(Vec<u8>, FileMode, Box<fs::Metadata>),
    /// Nothing that belongs in the tree: no file, or a directory, a socket.
    Missing,
    TooLarge,
    /// A file that is being written: to be looked at again.
    Later,
}

/// Time left before something modified at the time `meta` says has settled.
fn settle_left(meta: &fs::Metadata) -> Option<Duration> {
    // A date in the future says nothing: the file is taken as it is.
    let age = meta.modified().ok()?.elapsed().ok()?;
    SETTLE.checked_sub(age).filter(|left| !left.is_zero())
}

/// Reads a file for a capture, making sure it is not caught in the middle
/// of a save. The check is made on the very metadata the read starts from,
/// and again after the read: a file emptied an instant ago, or changed
/// while it was being read, is not what its author means to save.
/// `tracked`: the tree has a file at this path.
///
/// `whole`: what the file system said of the file when the system reported
/// it closed after being written, or put in place by a rename. A file that
/// is still as it was then is whole, however recent: there is nothing to
/// wait for.
fn read_settled(
    abs: &Path,
    tracked: bool,
    max_bytes: u64,
    unsettled: Unsettled,
    writing: &Writing,
    whole: Option<&Option<FileStat>>,
) -> OnDisk {
    // Bounded: a file that never stops changing is, in the end, read as it is.
    const ATTEMPTS: usize = 1200;
    for attempt in 1..=ATTEMPTS {
        // What to do about something unsettled: `true` to look again.
        let retry = |left: Duration| match unsettled {
            Unsettled::Wait if attempt < ATTEMPTS => {
                std::thread::sleep(left);
                Some(true)
            }
            Unsettled::Defer => Some(false),
            _ => None,
        };
        // Being written right now, by the events' account.
        if writing.is_open(abs) {
            match retry(Duration::from_millis(2)) {
                Some(true) => continue,
                Some(false) => return OnDisk::Later,
                None => {}
            }
        }
        match fs::symlink_metadata(abs) {
            Ok(meta) if meta.is_file() => {
                if meta.len() > max_bytes {
                    return OnDisk::TooLarge;
                }
                let vouched = whole.is_some_and(|then| *then == Some(FileStat::of(&meta)));
                if let Some(again) = settle_left(&meta).filter(|_| !vouched).and_then(retry) {
                    match again {
                        true => continue,
                        false => return OnDisk::Later,
                    }
                }
                // Windows has no executable bit: the capture keeps the mode
                // the tree had (ADR 0002).
                #[cfg(unix)]
                let mode = match meta.permissions().mode() & 0o111 {
                    0 => FileMode::Regular,
                    _ => FileMode::Executable,
                };
                #[cfg(not(unix))]
                let mode = FileMode::Regular;
                // If the file disappears between the stat and the read, it is a removal.
                let Ok(content) = fs::read(abs) else { return OnDisk::Missing };
                let unchanged =
                    fs::symlink_metadata(abs).is_ok_and(|after| FileStat::of(&after) == FileStat::of(&meta));
                if !unchanged && unsettled != Unsettled::Trust {
                    match retry(Duration::from_millis(1)) {
                        Some(true) => continue,
                        Some(false) => return OnDisk::Later,
                        None => {}
                    }
                }
                return OnDisk::File(content, mode, Box::new(meta));
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                return match fs::read_link(abs) {
                    Ok(target) => {
                        let target = target.to_string_lossy().into_owned().into_bytes();
                        OnDisk::File(target, FileMode::Symlink, Box::new(meta))
                    }
                    Err(_) => OnDisk::Missing,
                };
            }
            // Directories, sockets, fifos: they are not files of the tree.
            Ok(_) => return OnDisk::Missing,
            Err(_) => {
                // A tracked file that is not there, in a directory that
                // changed an instant ago: it may be between the removal and
                // the creation of a save that replaces it. Unless the
                // system said it was removed or renamed away, and nothing
                // has been said of it since.
                let said_gone = whole == Some(&None);
                let directory = abs.parent().and_then(|dir| fs::symlink_metadata(dir).ok());
                let unsettled_dir =
                    directory.as_ref().and_then(settle_left).filter(|_| tracked && !said_gone);
                match unsettled_dir.and_then(retry) {
                    Some(true) => continue,
                    Some(false) => return OnDisk::Later,
                    None => return OnDisk::Missing,
                }
            }
        }
    }
    OnDisk::Missing
}

/// What a capture does with a file that has not settled yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unsettled {
    /// Waits for it: the capture is a barrier, what is on disk when it
    /// returns is in the log.
    Wait,
    /// Leaves it for the next capture: file events are being followed, and
    /// more will come.
    Defer,
    /// Reads it as it is: the daemon itself has just written it.
    Trust,
}

/// Layout version of the saved state of the main lane.
const STATE_VERSION: u32 = 2;

/// Size, modification time and permissions of a file at the last capture:
/// if they have not changed, the scan does not reread it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileStat {
    size: u64,
    mtime_ns: i64,
    mode: u32,
}

impl FileStat {
    #[cfg(unix)]
    pub(crate) fn of(meta: &fs::Metadata) -> FileStat {
        FileStat {
            size: meta.len(),
            mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            mode: meta.mode(),
        }
    }

    /// On Windows: the modification time to the 100 ns the system keeps,
    /// and the attributes in place of a mode (ADR 0002).
    #[cfg(windows)]
    pub(crate) fn of(meta: &fs::Metadata) -> FileStat {
        use std::os::windows::fs::MetadataExt;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos() as i64);
        FileStat { size: meta.len(), mtime_ns, mode: meta.file_attributes() }
    }
}

/// State of the main lane saved to disk for a quick restart: it can always
/// be rebuilt by reapplying the ops.
#[derive(Serialize, Deserialize)]
struct SavedState {
    /// Layout of this file: a state written by another version is ignored
    /// and rebuilt from the log.
    #[serde(default)]
    version: u32,
    /// Ops of the main lane already applied to `tree` and `symbols`.
    ops: u64,
    head: Option<OpId>,
    tree: Tree,
    #[serde(default)]
    symbols: SymbolTable,
    /// See [`Repo::last_clean`].
    #[serde(default)]
    last_clean: Vec<(PathBuf, BlobId)>,
    stat: Vec<(PathBuf, FileStat)>,
    saved_ns: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    /// Capture batches that produced at least one op.
    pub captures: u64,
    pub ops: u64,
    pub scans: u64,
    pub skipped_too_large: u64,
    pub skipped_not_utf8: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub root: PathBuf,
    /// `Name <email>` of whoever signs the changes.
    pub author: String,
    pub identity: Identity,
    pub head: Option<String>,
    pub ops: u64,
    pub files: usize,
    /// Symbols (functions, classes…) the history knows in those files.
    #[serde(default)]
    pub symbols: usize,
    pub checkpoints: usize,
    pub last_checkpoint: Option<String>,
    /// Checkpoints not yet exported to git.
    pub unexported: usize,
    /// Ops recorded after the last checkpoint.
    pub ops_since_checkpoint: u64,
    pub counters: Counters,
    /// The main lane changes only when a lane is integrated.
    #[serde(default)]
    pub protected: bool,
    /// The commands that must succeed before a lane is integrated.
    #[serde(default)]
    pub checks: Vec<String>,
    /// How they run: `confined`, `confined, with network`, `off`, or
    /// `unavailable` (this system cannot confine them).
    #[serde(default)]
    pub sandbox: String,
    /// The git remote every integration is pushed to.
    #[serde(default)]
    pub push: Option<String>,
    /// Direct edits to a protected main lane put back since the daemon
    /// started, and the last of them.
    #[serde(default)]
    pub refused: u64,
    #[serde(default)]
    pub last_refused: Option<Refusal>,
    /// The last checkpoint known to be on the git remote, and why the last
    /// push failed, if it did.
    #[serde(default)]
    pub pushed: Option<String>,
    #[serde(default)]
    pub push_error: Option<String>,
    /// The lane where commits of the git remote wait to be merged.
    #[serde(default)]
    pub pull_waiting: Option<String>,
}

/// A file found changed in the directory of a protected main lane: its
/// path, and what was written there (`None`: it was removed).
type DirectEdit = (PathBuf, Option<(Vec<u8>, FileMode)>);

/// A direct edit to a protected main lane, put back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The files that were put back as the main lane has them.
    pub paths: Vec<String>,
    /// Where the edits are kept; `None` when files were only removed.
    pub kept: Option<PathBuf>,
}

/// `2026-11-17T06-30-12.123Z`: a moment as the name of a directory.
fn stamp(ms: u64) -> String {
    let (secs, millis) = (ms / 1000, ms % 1000);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // The civil date of a day counted from 1970-01-01.
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// One line of `kito log`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogItem {
    Op {
        id: String,
        at_ms: u64,
        author: String,
        lane: String,
        op: String,
        path: Option<String>,
        detail: Option<String>,
        lines_added: Option<usize>,
        lines_removed: Option<usize>,
        /// The "why" of the operation, if declared.
        #[serde(default)]
        intent: Option<String>,
        /// For a symbolic op: the ID of the file change it was recorded
        /// with. Absent when it had none (the first analysis of files that
        /// were already there).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change: Option<String>,
        /// For a symbolic op: the ID of the symbol, and the name it was
        /// given by this op (created, renamed) or has now.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
        /// For a rename: the name the symbol had before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        renamed_from: Option<String>,
    },
    Checkpoint {
        id: String,
        label: String,
        author: String,
        head: String,
        git_commit: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointResult {
    pub id: String,
    pub label: String,
    pub head: String,
    pub git_commit: Option<String>,
    /// Why the export to git failed, if it was attempted.
    pub git_error: Option<String>,
}

/// One entry of what a rollback changes in the working directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackChange {
    pub path: String,
    /// `restore`, `recreate` or `remove`.
    pub action: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackResult {
    /// The point being returned to, in words.
    pub target: String,
    pub changes: Vec<RollbackChange>,
    /// `false` for a dry run (`--dry-run`).
    pub applied: bool,
    /// Ops recorded by the rollback.
    pub ops: usize,
    /// Point to return to in order to undo this rollback.
    pub undo: Option<String>,
}

/// One lane at work: its working directory and what the log says of it.
pub(crate) struct Work {
    pub(crate) lane: LaneId,
    /// The working directory the lane is captured from.
    pub(crate) root: PathBuf,
    /// Who signs the ops captured there.
    pub(crate) author: Author,
    pub(crate) ignore: IgnoreRules,
    pub(crate) tree: Tree,
    pub(crate) symbols: SymbolTable,
    /// For files whose current content has syntax errors: the last content
    /// that parsed, to compare the next clean save with. A hint, not state:
    /// without it only the names of the symbols are compared.
    pub(crate) last_clean: HashMap<PathBuf, BlobId>,
    /// The parent of the next op.
    pub(crate) head: Option<OpId>,
    /// Ops of the lane, all applied to `tree` and `symbols`.
    pub(crate) ops: u64,
    pub(crate) stat: HashMap<PathBuf, FileStat>,
    /// The intent captured ops are recorded with, if one was declared.
    pub(crate) intent: Option<IntentId>,
    /// A second parent for the next op recorded: the head of the lane whose
    /// work that op brings in.
    pub(crate) merge_parent: Option<OpId>,
    /// Files a capture found still being written: the next one looks again.
    pub(crate) deferred: BTreeSet<PathBuf>,
    /// For the capture in progress: the files the system said were whole
    /// (closed after being written, put in place by a rename) and what it
    /// said of them then; `None` for one that was gone.
    settled: HashMap<PathBuf, Option<FileStat>>,
    /// Symbols removed lately: if one comes back, it gets its ID again.
    pub(crate) gone: Gone,
}

/// The symbols a lane removed lately, with where they were.
#[derive(Default)]
pub(crate) struct Gone {
    symbols: VecDeque<(SymbolId, kito_core::SymbolEntry)>,
}

impl Gone {
    /// How many removed symbols are remembered.
    const KEPT: usize = 20_000;

    fn note(&mut self, id: SymbolId, entry: kito_core::SymbolEntry) {
        if self.symbols.len() == Self::KEPT {
            self.symbols.pop_front();
        }
        self.symbols.push_back((id, entry));
    }

    fn forget(&mut self, id: SymbolId) {
        self.symbols.retain(|(gone, _)| *gone != id);
    }

    pub(crate) fn clear(&mut self) {
        self.symbols.clear();
    }

    /// Tells the differ, for the files a capture touches.
    fn tell(&self, identities: &mut Identities, files: &BTreeSet<&Path>) {
        // The most recent removal of a name is the first to come back.
        for (id, entry) in self.symbols.iter().rev() {
            if files.contains(entry.file.as_path()) {
                identities.gone(&entry.file, entry.parent, entry.kind, &entry.name, *id);
            }
        }
    }
}

impl Work {
    pub(crate) fn new(lane: LaneId, root: PathBuf, author: Author, ignore: &[String]) -> Work {
        Work {
            lane,
            ignore: IgnoreRules::new(&root, ignore),
            root,
            author,
            tree: Tree::new(),
            symbols: SymbolTable::new(),
            last_clean: HashMap::new(),
            head: None,
            ops: 0,
            stat: HashMap::new(),
            intent: None,
            merge_parent: None,
            deferred: BTreeSet::new(),
            settled: HashMap::new(),
            gone: Gone::default(),
        }
    }

    /// A stand-in for a lane that is being worked on elsewhere.
    fn hollow() -> Work {
        Work::new(LaneId::MAIN, PathBuf::new(), Author::human(""), &[])
    }
}

pub struct Repo {
    pub(crate) root: PathBuf,
    pub(crate) store: Store,
    pub(crate) config: Config,
    pub(crate) identity: Identity,
    /// The main lane: the repository's own directory.
    pub(crate) main: Work,
    /// The open lanes of the agents.
    pub(crate) lanes: BTreeMap<LaneId, crate::lanes::LaneAt>,
    /// Parsed versions of the files captured most recently.
    pub(crate) parse_cache: semantic::ParseCache,
    /// The last checkpoint exported to git while it was the head of the
    /// main lane, and the files at that point.
    exported: Option<(kito_core::CheckpointId, Tree)>,
    /// Who knows about the references between symbols (level 2).
    semantic: std::sync::Arc<kito_semantic::Semantic>,
    /// A checkpoint was exported to git without rewriting git's index.
    index_stale: bool,
    pub(crate) clock: HlcClock,
    pub(crate) counters: Counters,
    /// Files being written right now, kept up to date by whoever follows
    /// the file events.
    pub(crate) writing: std::sync::Arc<Writing>,
    /// What each lane declared and changed, as last computed.
    pub(crate) claims_cache: crate::scope::ClaimsCache,
    /// What each lane was already told about the other lanes.
    pub(crate) told: BTreeMap<LaneId, std::collections::BTreeSet<String>>,
    /// While `true`, the main lane may change though it is protected: an
    /// integration is writing its result.
    pub(crate) main_open: bool,
    /// Direct edits to a protected main lane that were put back, since
    /// somebody last asked.
    pub(crate) refused: Vec<Refusal>,
    /// How many there were since the repository was opened, and the last.
    pub(crate) refusals: (u64, Option<Refusal>),
    /// The last checkpoint pushed to the git remote, and why the last push
    /// failed.
    pub(crate) pushed: (Option<String>, Option<String>),
    /// The reasons each lane was told, so that none is told twice.
    pub(crate) reasons_told: HashMap<LaneId, BTreeSet<String>>,
    /// The last direct edits of the main directory, put back or not.
    pub(crate) direct: Vec<crate::guard::Direct>,
    /// From reasons to what they touched: built when somebody first asks
    /// why, kept up to date from then on.
    pub(crate) why_index: Option<crate::why::WhyIndex>,
    /// What was recorded since somebody last asked: for whoever tells the
    /// clients that watch the repository.
    pub(crate) recorded: Vec<Op>,
}

/// How many recorded ops are kept for whoever asks: with nobody asking,
/// the oldest are forgotten.
const RECORDED_KEPT: usize = 4096;

fn hex20(oid: &[u8; 20]) -> String {
    oid.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

fn file_changes(n: usize) -> String {
    match n {
        1 => "1 file change".to_owned(),
        n => format!("{n} file changes"),
    }
}

/// The ops of a lane, newest first, read a page at a time.
struct Backwards<'a> {
    store: &'a Store,
    lane: LaneId,
    end: u64,
    page: Vec<Op>,
}

impl Backwards<'_> {
    fn next(&mut self) -> Result<Option<Op>> {
        if self.page.is_empty() && self.end > 0 {
            let start = self.end.saturating_sub(500);
            self.page = self.store.lane_ops(self.lane, start, (self.end - start) as usize)?;
            self.end = start;
        }
        Ok(self.page.pop())
    }
}

impl Repo {
    /// Creates `.kito/` in `root` with an empty store and the default configuration.
    pub fn init(root: &Path) -> Result<Repo> {
        let store_dir = root.join(STORE_DIR);
        drop(Store::init(&store_dir)?);
        let config = store_dir.join(CONFIG_FILE);
        if !config.exists() {
            atomic_write(&config, DEFAULT_CONFIG.as_bytes()).map_err(Error::io(&config))?;
        }
        Repo::open(root)
    }

    /// Opens the repository in `root` and rebuilds the state of the main
    /// lane and of the open lanes.
    pub fn open(root: &Path) -> Result<Repo> {
        let root = root.canonicalize().map_err(Error::io(root))?;
        let store_dir = root.join(STORE_DIR);
        let store = Store::open(&store_dir)?;
        let config = Config::load(&store_dir).map_err(Error::Config)?;
        let providers = crate::refs::providers(&config, &root);
        let identity = identity::resolve(&config, &root);
        let main = Work::new(LaneId::MAIN, root.clone(), identity.author(), &config.capture.ignore);
        let mut repo = Repo {
            root,
            store,
            config,
            identity,
            main,
            lanes: BTreeMap::new(),
            parse_cache: semantic::ParseCache::default(),
            exported: None,
            semantic: std::sync::Arc::new(providers),
            index_stale: false,
            clock: HlcClock::new(),
            counters: Counters::default(),
            writing: std::sync::Arc::default(),
            claims_cache: Default::default(),
            told: BTreeMap::new(),
            main_open: false,
            refused: Vec::new(),
            refusals: (0, None),
            reasons_told: HashMap::new(),
            direct: Vec::new(),
            pushed: (None, None),
            why_index: None,
            recorded: Vec::new(),
        };
        repo.load_state()?;
        repo.load_lanes()?;
        // The clock restarts after the last op of any lane.
        let mut last = Hlc::ZERO;
        let heads = repo.lanes.values().map(|at| at.work.head).chain([repo.main.head]);
        for head in heads.flatten().collect::<Vec<_>>() {
            last = last.max(repo.store.get_op(&head)?.map_or(Hlc::ZERO, |op| op.at));
        }
        if last > Hlc::ZERO {
            repo.clock = HlcClock::starting_after(last);
        }
        Ok(repo)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where whoever follows the file events notes which files are being
    /// written: captures leave those alone until they are closed.
    pub fn writing(&self) -> std::sync::Arc<Writing> {
        self.writing.clone()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Who signs the changes captured in this repository.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Rereads `config.toml`: identity and ignore rules take effect right
    /// away, without restarting the daemon.
    pub fn reload_config(&mut self) -> Result<()> {
        self.config = Config::load(&self.root.join(STORE_DIR)).map_err(Error::Config)?;
        self.reload_providers();
        self.main.ignore = IgnoreRules::new(&self.root, &self.config.capture.ignore);
        self.identity = identity::resolve(&self.config, &self.root);
        self.main.author = self.identity.author();
        let ignore = self.lane_ignore();
        for at in self.lanes.values_mut() {
            at.work.ignore = IgnoreRules::new(&at.work.root, &ignore);
        }
        Ok(())
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// State of the main lane: path → blob.
    pub fn tree(&self) -> &Tree {
        &self.main.tree
    }

    pub fn head(&self) -> Option<OpId> {
        self.main.head
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(STORE_DIR).join("lanes").join("main").join("state")
    }

    /// Loads the saved state, if it still matches the log, and reapplies the
    /// later ops. Without a saved state it reapplies everything from the start.
    fn load_state(&mut self) -> Result<()> {
        let total = self.store.lane_len(LaneId::MAIN)?;
        let saved =
            fs::read(self.state_path()).ok().and_then(|bytes| codec::from_slice::<SavedState>(&bytes).ok());
        if let Some(saved) = saved {
            let matches_log = saved.ops <= total
                && match saved.ops {
                    0 => saved.head.is_none(),
                    n => self.store.lane_ops(LaneId::MAIN, n - 1, 1)?.first().map(|op| op.id) == saved.head,
                };
            if matches_log && saved.version == STATE_VERSION {
                self.main.tree = saved.tree;
                self.main.symbols = saved.symbols;
                self.main.last_clean = saved.last_clean.into_iter().collect();
                self.main.head = saved.head;
                self.main.ops = saved.ops;
                let racy_after = saved.saved_ns - RACY_WINDOW.as_nanos() as i64;
                self.main.stat = saved.stat.into_iter().filter(|(_, s)| s.mtime_ns < racy_after).collect();
            }
        }
        while self.main.ops < total {
            let page = self.store.lane_ops(LaneId::MAIN, self.main.ops, 1000)?;
            if page.is_empty() {
                break;
            }
            for op in page {
                for path in op.kind.paths() {
                    self.main.stat.remove(path);
                }
                self.main.tree.apply(&op.kind)?;
                self.main.symbols.apply(&op.kind)?;
                self.main.head = Some(op.id);
                self.main.ops += 1;
            }
        }
        Ok(())
    }

    /// Saves the state of the main lane for the next open.
    pub fn save_state(&self) -> Result<()> {
        let saved = SavedState {
            version: STATE_VERSION,
            ops: self.main.ops,
            head: self.main.head,
            tree: self.main.tree.clone(),
            symbols: self.main.symbols.clone(),
            last_clean: self.main.last_clean.iter().map(|(p, b)| (p.clone(), *b)).collect(),
            stat: self.main.stat.iter().map(|(p, s)| (p.clone(), *s)).collect(),
            saved_ns: now_ns(),
        };
        let path = self.state_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(Error::io(dir))?;
        }
        atomic_write(&path, &codec::to_vec(&saved)?).map_err(Error::io(&path))
    }

    /// Path relative to the root, if `path` is inside the repository.
    pub fn relative(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(&self.root).ok().map(kito_core::paths::portable_path)
    }

    /// Full scan of the main working directory: compares it with the tree
    /// and captures every difference. It is also the barrier of `sync`: when
    /// it returns, everything that was on disk is in the log.
    pub fn scan(&mut self) -> Result<Vec<Op>> {
        self.scan_lane(LaneId::MAIN)
    }

    /// The same, for the working copy of a lane.
    pub fn scan_lane(&mut self, lane: LaneId) -> Result<Vec<Op>> {
        self.with(lane, |repo, work| repo.scan_work(work))
    }

    /// Runs `f` on a lane and on the rest of the repository at once: the
    /// lane is taken out while `f` runs, and put back whatever happens.
    pub(crate) fn with<T>(
        &mut self,
        lane: LaneId,
        f: impl FnOnce(&mut Repo, &mut Work) -> Result<T>,
    ) -> Result<T> {
        let mut work = match lane.is_main() {
            true => std::mem::replace(&mut self.main, Work::hollow()),
            false => {
                let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
                std::mem::replace(&mut at.work, Work::hollow())
            }
        };
        let result = f(self, &mut work);
        match lane.is_main() {
            true => self.main = work,
            false => {
                if let Some(at) = self.lanes.get_mut(&lane) {
                    at.work = work;
                }
            }
        }
        result
    }

    fn scan_work(&mut self, work: &mut Work) -> Result<Vec<Op>> {
        self.counters.scans += 1;
        // The names first: reading directories is quick. What takes time in
        // a large repository is asking the file system about every file,
        // and that is done by several threads.
        let mut found = Vec::new();
        Self::walk(work, Path::new(""), &mut found);
        let (mut candidates, tracked_seen) = {
            let work: &Work = work;
            let look = |files: &[PathBuf]| {
                let mut candidates = Vec::new();
                let mut tracked_seen = 0usize;
                for rel in files {
                    let stat = fs::symlink_metadata(work.root.join(rel)).ok().map(|meta| FileStat::of(&meta));
                    match work.stat.get(rel) {
                        // Only what the stat cache does not vouch for is
                        // looked at closely. The cache holds tracked files
                        // only: a hit is also a tracked file seen.
                        Some(known) if Some(known) == stat.as_ref() => tracked_seen += 1,
                        Some(_) => {
                            tracked_seen += 1;
                            candidates.push(rel.clone());
                        }
                        // A tracked file stays tracked whatever the ignore
                        // rules say now.
                        None if work.tree.contains(rel) => {
                            tracked_seen += 1;
                            candidates.push(rel.clone());
                        }
                        None if !work.ignore.is_ignored(rel, false) => candidates.push(rel.clone()),
                        None => {}
                    }
                }
                (candidates, tracked_seen)
            };
            let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).min(8);
            if found.len() < 4096 || workers < 2 {
                look(&found)
            } else {
                let look = &look;
                std::thread::scope(|scope| {
                    let handles: Vec<_> = found
                        .chunks(found.len().div_ceil(workers))
                        .map(|part| scope.spawn(move || look(part)))
                        .collect();
                    let mut all = (Vec::new(), 0usize);
                    for handle in handles {
                        let (candidates, tracked_seen) = handle.join().expect("a scan worker panicked");
                        all.0.extend(candidates);
                        all.1 += tracked_seen;
                    }
                    all
                })
            }
        };
        // Files of the tree that the walk did not come across: removed, or
        // inside a directory an ignore rule now covers (in which case they
        // stay tracked).
        if tracked_seen < work.tree.len() {
            let seen: HashSet<&Path> = found.iter().map(PathBuf::as_path).collect();
            candidates.extend(
                work.tree.iter().filter(|(path, _)| !seen.contains(path)).map(|(path, _)| path.to_owned()),
            );
        }
        let intent = work.intent;
        self.capture_work(work, candidates, intent, &Identities::new(), Unsettled::Wait)
    }

    /// Files and links under `dir`, without following links and without
    /// entering ignored directories.
    fn walk(work: &Work, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(work.root.join(dir)) else { return };
        for entry in entries.flatten() {
            let rel = kito_core::paths::join_portable(dir, &entry.file_name());
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if !work.ignore.is_ignored(&rel, true) {
                    Self::walk(work, &rel, out);
                }
            } else if kind.is_file() || kind.is_symlink() {
                out.push(rel);
            }
        }
    }

    /// Captures the current state of the given paths of the main working
    /// directory (relative to the root) and records the resulting ops. A
    /// directory stands for everything it contains. Returns the recorded
    /// ops, already confirmed.
    pub fn capture(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> Result<Vec<Op>> {
        self.capture_lane(LaneId::MAIN, paths)
    }

    /// The same, for paths of the working copy of a lane.
    pub fn capture_lane(
        &mut self,
        lane: LaneId,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Vec<Op>> {
        self.with(lane, |repo, work| {
            let intent = work.intent;
            repo.capture_work(work, paths, intent, &Identities::new(), Unsettled::Wait)
        })
    }

    /// Captures paths reported by file events. A file that is still being
    /// written is left for later: see [`Repo::has_deferred`].
    pub fn capture_events(
        &mut self,
        lane: LaneId,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Vec<Op>> {
        self.with(lane, |repo, work| {
            let intent = work.intent;
            repo.capture_work(work, paths, intent, &Identities::new(), Unsettled::Defer)
        })
    }

    /// The same, for events that say more: `Some` for a path the system
    /// reported whole at that moment (closed after being written, put in
    /// place by a rename, removed), with what the file system said of it
    /// then, or `None` if it was gone. Such a file is captured at once if
    /// it has not changed since.
    pub(crate) fn capture_reported(
        &mut self,
        lane: LaneId,
        paths: impl IntoIterator<Item = (PathBuf, Option<Option<FileStat>>)>,
    ) -> Result<Vec<Op>> {
        self.with(lane, |repo, work| {
            let intent = work.intent;
            let mut all = Vec::new();
            for (path, whole) in paths {
                let path = kito_core::paths::portable_path(&path);
                if let Some(then) = whole {
                    work.settled.insert(path.clone(), then);
                }
                all.push(path);
            }
            let captured = repo.capture_work(work, all, intent, &Identities::new(), Unsettled::Defer);
            work.settled.clear();
            captured
        })
    }

    /// `true` if a capture left files for later because they were still
    /// being written.
    pub fn has_deferred(&self) -> bool {
        !self.main.deferred.is_empty() || self.lanes.values().any(|at| !at.work.deferred.is_empty())
    }

    /// Looks again at the files left for later.
    pub fn capture_deferred(&mut self) -> Result<Vec<Op>> {
        let waiting: Vec<LaneId> = std::iter::once(LaneId::MAIN)
            .filter(|_| !self.main.deferred.is_empty())
            .chain(self.lanes.iter().filter(|(_, at)| !at.work.deferred.is_empty()).map(|(lane, _)| *lane))
            .collect();
        let mut ops = Vec::new();
        for lane in waiting {
            ops.extend(self.capture_events(lane, [])?);
        }
        Ok(ops)
    }

    /// Like [`Repo::capture`], declaring the "why" of the recorded ops.
    fn capture_as(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        intent: Option<IntentId>,
    ) -> Result<Vec<Op>> {
        // The files were written by the rollback itself.
        self.with(LaneId::MAIN, |repo, work| {
            repo.capture_work(work, paths, intent, &Identities::new(), Unsettled::Trust)
        })
    }

    /// Captures paths of a lane's working directory. `identities`: symbols
    /// of the new contents whose identity is known in advance.
    pub(crate) fn capture_work(
        &mut self,
        work: &mut Work,
        paths: impl IntoIterator<Item = PathBuf>,
        intent: Option<IntentId>,
        identities: &Identities,
        unsettled: Unsettled,
    ) -> Result<Vec<Op>> {
        // 1. From the reported paths to the individual files to look at.
        let mut files: BTreeSet<PathBuf> = BTreeSet::new();
        for rel in paths {
            // However it was reported, with `/` (ADR 0002).
            let rel = kito_core::paths::portable_path(&rel);
            if rel.file_name().is_some_and(|n| n == ".gitignore" || n == ".kitoignore") {
                work.ignore.invalidate();
            }
            let tracked_below: Vec<PathBuf> = work.tree.under(&rel).map(|(p, _)| p.to_owned()).collect();
            match fs::symlink_metadata(work.root.join(&rel)) {
                Ok(meta) if meta.is_dir() => {
                    if !work.ignore.is_ignored(&rel, true) {
                        let mut found = Vec::new();
                        Self::walk(work, &rel, &mut found);
                        files.extend(found.into_iter().filter(|file| {
                            work.stat.contains_key(file)
                                || work.tree.contains(file)
                                || !work.ignore.is_ignored(file, false)
                        }));
                    }
                    files.extend(tracked_below);
                    // A file replaced by a directory with the same name.
                    if work.tree.contains(&rel) {
                        files.insert(rel);
                    }
                }
                _ => {
                    files.extend(tracked_below);
                    if work.tree.contains(&rel) || !work.ignore.is_ignored(&rel, false) {
                        files.insert(rel);
                    }
                }
            }
        }

        // Files an earlier capture found still being written.
        files.extend(std::mem::take(&mut work.deferred));

        // A protected main lane takes no change from its directory: what
        // differs there is put back, and kept aside.
        let protected = work.lane.is_main() && self.config.main.protected && !self.main_open;
        let mut put_back: Vec<DirectEdit> = Vec::new();

        // 2. On-disk state of each file.
        let mut changes: BTreeMap<PathBuf, Option<Snapshot>> = BTreeMap::new();
        let mut stats: Vec<(PathBuf, Option<FileStat>)> = Vec::new();
        for rel in files {
            if rel.as_os_str().is_empty() {
                continue;
            }
            if rel.to_str().is_none() {
                self.counters.skipped_not_utf8 += 1;
                continue;
            }
            let abs = work.root.join(&rel);
            let limit = self.config.capture.max_file_bytes;
            let whole = work.settled.get(&rel);
            let tracked = work.tree.contains(&rel);
            let on_disk = match read_settled(&abs, tracked, limit, unsettled, &self.writing, whole) {
                OnDisk::File(content, mode, meta) => Some((content, mode, *meta)),
                OnDisk::Missing => None,
                OnDisk::TooLarge => {
                    self.counters.skipped_too_large += 1;
                    continue;
                }
                OnDisk::Later => {
                    work.deferred.insert(rel);
                    continue;
                }
            };
            match on_disk {
                Some((content, mode, meta)) => {
                    // Without an executable bit or links (Windows), a file
                    // keeps the mode the tree gives it (ADR 0002).
                    #[cfg(not(unix))]
                    let mode = match (mode, work.tree.get(&rel).map(|entry| entry.mode)) {
                        (FileMode::Regular, Some(kept @ (FileMode::Executable | FileMode::Symlink))) => kept,
                        _ => mode,
                    };
                    let blob = BlobId::of(&content);
                    let same = work.tree.get(&rel).is_some_and(|e| e.blob == blob && e.mode == mode);
                    if !same && protected {
                        put_back.push((rel, Some((content, mode))));
                        continue;
                    }
                    if !same {
                        // A file saved again is stored as what changed
                        // since the version the lane had.
                        match work.tree.get(&rel) {
                            Some(before) => self.store.put_blob_like(&content, &before.blob)?,
                            None => self.store.put_blob(&content)?,
                        };
                        changes.insert(rel.clone(), Some(Snapshot { blob, mode }));
                    }
                    let fresh = meta
                        .modified()
                        .ok()
                        .and_then(|m| m.elapsed().ok())
                        .is_none_or(|age| age < RACY_WINDOW);
                    stats.push((rel, (!fresh).then(|| FileStat::of(&meta))));
                }
                None if protected && work.tree.contains(&rel) => put_back.push((rel, None)),
                None => {
                    if work.tree.contains(&rel) {
                        changes.insert(rel.clone(), None);
                    }
                    stats.push((rel, None));
                }
            }
        }
        if !put_back.is_empty() {
            self.put_back(work, put_back)?;
        }

        // 3. Ops, in an order that can be applied to the tree, chained to the head.
        let store = &self.store;
        let file_ops = ops_for_changes(&work.tree, &changes, &mut |blob| store.get_blob(blob).ok().flatten());
        // The file-level ops carry the new contents; what the change means
        // for the symbols follows.
        // Symbols that went away from these files and may be coming back.
        let mut identities = identities.clone();
        let touched: BTreeSet<&Path> =
            file_ops.iter().flat_map(|op| op.paths()).map(|p| p.as_path()).collect();
        work.gone.tell(&mut identities, &touched);
        let identities = &identities;
        let symbolic = semantic::symbol_ops(
            &self.store,
            &work.tree,
            &work.symbols,
            work.lane,
            &file_ops,
            &mut work.last_clean,
            &mut self.parse_cache,
            identities,
        )?;
        for blob in &symbolic.blobs {
            self.store.put_blob(blob)?;
        }
        // What changed in the main directory without an integration
        // writing there was changed by hand.
        let by_hand: Vec<String> = match work.lane.is_main() && !self.main_open {
            true => {
                file_ops.iter().flat_map(|op| op.paths()).map(|path| path.display().to_string()).collect()
            }
            false => Vec::new(),
        };
        let ops = self.record_work(work, file_ops.into_iter().chain(symbolic.ops), intent)?;
        if !by_hand.is_empty() && !ops.is_empty() {
            let at_ms =
                SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64);
            self.note_direct(crate::guard::Direct { at_ms, paths: by_hand, put_back: false, kept: None });
        }
        if !ops.is_empty() {
            self.store.commit()?;
            self.counters.captures += 1;
            self.counters.ops += ops.len() as u64;
            self.recorded.extend(ops.iter().cloned());
            if self.recorded.len() > RECORDED_KEPT {
                let extra = self.recorded.len() - RECORDED_KEPT;
                self.recorded.drain(..extra);
            }
        }
        for (rel, stat) in stats {
            match stat {
                Some(stat) => work.stat.insert(rel, stat),
                None => work.stat.remove(&rel),
            };
        }
        Ok(ops)
    }

    /// Puts files of a protected main lane back as the lane has them. What
    /// was written in their place is kept under `.kito/refused/`: nothing
    /// anybody typed is thrown away.
    fn put_back(&mut self, work: &mut Work, edits: Vec<DirectEdit>) -> Result<()> {
        let at_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64);
        let aside = self.root.join(STORE_DIR).join("refused").join(stamp(at_ms));
        let mut kept = false;
        let mut paths = Vec::new();
        for (rel, edit) in edits {
            if let Some((content, mode)) = &edit {
                write_file(&aside, &rel, *mode, content)?;
                kept = true;
            }
            match work.tree.get(&rel).copied() {
                Some(entry) => self.write_file_in(&work.root, &rel, &entry)?,
                None => crate::integrate::remove_in(&work.root, &rel)?,
            }
            work.stat.remove(&rel);
            paths.push(rel.display().to_string());
        }
        let refusal = Refusal { at_ms, paths, kept: kept.then_some(aside) };
        self.note_direct(crate::guard::Direct {
            at_ms,
            paths: refusal.paths.clone(),
            put_back: true,
            kept: refusal.kept.clone(),
        });
        self.refusals = (self.refusals.0 + 1, Some(refusal.clone()));
        self.refused.push(refusal);
        if self.refused.len() > 256 {
            self.refused.remove(0);
        }
        Ok(())
    }

    /// Turns the protection of the main lane on or off. Turned on, what
    /// the main directory holds that is in no checkpoint yet gets one: the
    /// last state anybody put there by hand. Returns `false` if it was
    /// already so.
    pub fn protect(&mut self, on: bool) -> Result<bool> {
        if self.config.main.protected == on {
            return Ok(false);
        }
        if on {
            self.scan()?;
            // Only for files: what Kito worked out of them by itself (their
            // symbols) goes with the next integration.
            let since = self.status()?.ops_since_checkpoint;
            let pending = self.store.lane_ops(LaneId::MAIN, self.main.ops - since, since as usize)?;
            if pending.iter().any(|op| op.kind.symbol().is_none()) {
                self.checkpoint("before the main lane was protected")?;
            }
        }
        crate::config::write_value(&self.root.join(STORE_DIR), "main", "protected", Some(on.into()))
            .map_err(Error::Config)?;
        self.config.main.protected = on;
        Ok(true)
    }

    /// Sets the rules a repository applies to integrations: the commands
    /// that must succeed (`checks`) and the git remote each integration is
    /// pushed to (`push`, `Some(None)` to stop). What is `None` stays.
    pub fn configure(&mut self, checks: Option<Vec<String>>, push: Option<Option<String>>) -> Result<()> {
        let store = self.root.join(STORE_DIR);
        if let Some(checks) = checks {
            let checks: Vec<String> =
                checks.into_iter().map(|c| c.trim().to_owned()).filter(|c| !c.is_empty()).collect();
            let list: toml_edit::Array = checks.iter().map(String::as_str).collect();
            crate::config::write_value(&store, "verify", "commands", Some(list.into()))
                .map_err(Error::Config)?;
            self.config.verify.commands = checks;
        }
        if let Some(push) = push {
            let push = push.map(|remote| remote.trim().to_owned()).filter(|remote| !remote.is_empty());
            crate::config::write_value(&store, "git", "push", push.as_deref().map(Into::into))
                .map_err(Error::Config)?;
            self.config.git.push = push;
            self.pushed = (None, None);
        }
        Ok(())
    }

    /// What a push to the git remote came to.
    pub fn set_pushed(&mut self, checkpoint: Option<String>, error: Option<String>) {
        if error.is_none() {
            self.pushed.0 = checkpoint;
        }
        self.pushed.1 = error;
    }

    /// Says no to what changes a protected main lane from outside a lane.
    pub(crate) fn main_is_open_to(&self, what: &str) -> Result<()> {
        match self.config.main.protected {
            true => Err(Error::Refused(format!(
                "the main lane is protected: it changes only when a lane is integrated, and {what} is not                  that. Do it in a lane, or turn the protection off first (`kito protect --off`)"
            ))),
            false => Ok(()),
        }
    }

    /// Appends ops to a lane, chained to its head, and applies them to its
    /// state. They are confirmed by the caller's commit.
    pub(crate) fn record_work(
        &mut self,
        work: &mut Work,
        kinds: impl IntoIterator<Item = OpKind>,
        intent: Option<IntentId>,
    ) -> Result<Vec<Op>> {
        let mut ops = Vec::new();
        for kind in kinds {
            let parents = work.head.into_iter().chain(work.merge_parent.take());
            let op = Op::new(parents, work.lane, work.author.clone(), self.clock.now(), kind, intent)?;
            match &op.kind {
                OpKind::RemoveSymbol { id } => {
                    if let Some(entry) = work.symbols.get(*id) {
                        work.gone.note(*id, entry.clone());
                    }
                }
                OpKind::AddSymbol { sym, .. } => work.gone.forget(sym.id),
                _ => {}
            }
            work.tree.apply(&op.kind)?;
            work.symbols.apply(&op.kind)?;
            // An op the store has already is not recorded twice, nor counted.
            if self.store.append_op(&op)? {
                if let Some(index) = &mut self.why_index {
                    index.add(&op);
                }
            }
            work.head = Some(op.id);
            work.ops += 1;
            ops.push(op);
        }
        Ok(ops)
    }

    /// The same, on the main lane.
    fn record(
        &mut self,
        kinds: impl IntoIterator<Item = OpKind>,
        intent: Option<IntentId>,
    ) -> Result<Vec<Op>> {
        self.with(LaneId::MAIN, |repo, work| repo.record_work(work, kinds, intent))
    }

    /// Analyzes the files the history knows only as text (imported from git,
    /// or captured before symbols existed) and records their symbols. Does
    /// nothing once every parseable file has been analyzed.
    pub fn analyze(&mut self) -> Result<usize> {
        let found =
            semantic::analyze_unindexed(&self.store, &self.main.tree, &self.main.symbols, LaneId::MAIN)?;
        if found.ops.is_empty() {
            return Ok(0);
        }
        for blob in &found.blobs {
            self.store.put_blob(blob)?;
        }
        let intent = Intent::new("first analysis of the symbols", None, None)?;
        self.store.put_intent(&intent)?;
        let ops = self.record(found.ops, Some(intent.id))?;
        self.store.commit()?;
        self.counters.ops += ops.len() as u64;
        Ok(ops.len())
    }

    /// The symbols the history knows.
    pub fn symbols(&self) -> &SymbolTable {
        &self.main.symbols
    }

    /// The history of the symbol called `name` in `file`, across renames
    /// and moves.
    pub fn why(&self, file: &Path, name: &str) -> Result<Vec<Why>> {
        self.why_lane(LaneId::MAIN, file, name)
    }

    /// The same, as a lane knows the symbol.
    pub fn why_lane(&self, lane: LaneId, file: &Path, name: &str) -> Result<Vec<Why>> {
        semantic::why(&self.store, &self.work(lane)?.symbols, lane, file, name)
    }

    /// Scans the main working directory and every open lane's.
    pub fn scan_all(&mut self) -> Result<Vec<Op>> {
        let mut ops = self.scan()?;
        for lane in self.lanes.keys().copied().collect::<Vec<_>>() {
            ops.extend(self.scan_lane(lane)?);
        }
        Ok(ops)
    }

    /// Creates a checkpoint on the current state of the working directory
    /// and, if configured, exports it to git.
    pub fn checkpoint(&mut self, label: &str) -> Result<CheckpointResult> {
        self.scan()?;
        let head = self.main.head.ok_or(Error::NothingToCheckpoint)?;
        // On a protected main lane every integration is a checkpoint, and
        // nothing else changes it.
        if self.config.main.protected && self.store.checkpoints().last().is_some_and(|last| last.head == head)
        {
            return Err(Error::Refused(
                "the main lane is protected: every integration is a checkpoint, and there is nothing                  else to put in one"
                    .into(),
            ));
        }
        let checkpoint = Checkpoint {
            id: self.store.next_checkpoint_id(),
            head,
            label: label.trim().to_owned(),
            created_by: self.main.author.clone(),
            verification: None,
            git_commit: None,
            approved_by: None,
            git_merge: None,
            lane: None,
        };
        self.store.put_checkpoint(&checkpoint)?;
        self.save_state()?;
        let mut result = CheckpointResult {
            id: checkpoint.id.to_string(),
            label: checkpoint.label.clone(),
            head: head.to_string(),
            git_commit: None,
            git_error: None,
        };
        if self.config.git.export_on_checkpoint {
            match self.export_git() {
                Ok(_) => {
                    result.git_commit =
                        self.store.checkpoint(checkpoint.id).and_then(|c| c.git_commit.as_ref()).map(hex20);
                }
                Err(e) => result.git_error = Some(e.to_string()),
            }
        }
        Ok(result)
    }

    /// Exports the checkpoints not yet exported to git.
    /// Builds the providers of level 2 again: the configuration changed, or
    /// an index was imported.
    pub(crate) fn reload_providers(&mut self) {
        self.semantic = std::sync::Arc::new(crate::refs::providers(&self.config, &self.root));
    }

    /// The providers of level 2: what to ask who uses a symbol.
    pub fn semantic(&self) -> std::sync::Arc<kito_semantic::Semantic> {
        self.semantic.clone()
    }

    pub fn export_git(&mut self) -> Result<Vec<(String, String)>> {
        self.export_git_with(true)
    }

    /// Exports like [`Repo::export_git`] but leaves git's index as it is:
    /// rewriting it costs in proportion to the repository, and whoever is
    /// in a hurry does it a moment later with [`Repo::refresh_git_index`].
    pub(crate) fn export_git_later_index(&mut self) -> Result<Vec<(String, String)>> {
        self.export_git_with(false)
    }

    /// What is left to do after an export that did not rewrite git's index:
    /// where the repository is and how it is exported. `None` if nothing
    /// is. The caller passes them to `kito_git::refresh_index`; the
    /// repository need not be held meanwhile.
    pub fn take_index_refresh(&mut self) -> Option<(PathBuf, kito_git::ExportOptions)> {
        std::mem::take(&mut self.index_stale).then(|| (self.root.clone(), self.config.export_options()))
    }

    /// Rewrites git's index if an export left it behind.
    pub fn refresh_git_index(&mut self) -> Result<()> {
        if let Some((root, options)) = self.take_index_refresh() {
            kito_git::refresh_index(&root, &options)?;
        }
        Ok(())
    }

    /// Exports the checkpoints of a permanent lane not exported yet to its
    /// own branch (`[lanes.<name>] branch`, else its name). Each commit
    /// comes after the lane's commit before it and, the first time the
    /// lane is on a new state of the main lane, after that state's commit
    /// too (ADR 0001).
    pub(crate) fn export_lane_git(&mut self, lane: LaneId) -> Result<Vec<(String, String)>> {
        let Some(name) = self.lanes.get(&lane).and_then(|at| at.record.name.clone()) else {
            return Ok(Vec::new());
        };
        let branch = self.config.lanes.permanent.get(&name).and_then(|c| c.branch.clone()).unwrap_or(name);
        let email = self.config.git.email_domain.clone();
        let mine: Vec<Checkpoint> = self.store.lane_checkpoints(lane).cloned().collect();
        let Some(first) = mine.iter().position(|ck| ck.git_commit.is_none()) else { return Ok(Vec::new()) };
        let before = first.checked_sub(1).map(|at| &mine[at]);
        let mut previous = before.and_then(|ck| ck.git_commit);
        let mut previous_on = match before {
            Some(ck) => self.main_commit_under(lane, ck.head)?,
            None => None,
        };
        let mut done = Vec::new();
        for ck in &mine[first..] {
            let (tree, _) = self.state_at(ck.head)?;
            let on = self.main_commit_under(lane, ck.head)?;
            let mut parents: Vec<[u8; 20]> = match previous {
                None => on.into_iter().collect(),
                Some(previous) => {
                    std::iter::once(previous).chain(on.filter(|on| Some(*on) != previous_on)).collect()
                }
            };
            // A catch-up says which commit it merged.
            if let Some(merged) = ck.git_merge.filter(|merged| !parents.contains(merged)) {
                parents.push(merged);
            }
            let wall_ms = self.store.get_op(&ck.head)?.map_or(0, |op| op.at.wall_ms);
            let commit = kito_git::LaneCommit { checkpoint: ck.clone(), tree, parents, wall_ms };
            let exported = kito_git::export_lane(&mut self.store, &self.root, &branch, &email, &[commit])?;
            for one in exported {
                previous = Some(one.commit);
                done.push((one.checkpoint.to_string(), hex20(&one.commit)));
            }
            previous_on = on;
        }
        Ok(done)
    }

    /// The commit of the main lane a lane is on at its op `head`: its
    /// first parents lead, through the lane, to the op of the main lane it
    /// was last brought up to. The checkpoint at that op, or else the one
    /// the lane is filed under.
    fn main_commit_under(&self, lane: LaneId, head: kito_core::OpId) -> Result<Option<[u8; 20]>> {
        let mut at = head;
        loop {
            let Some(op) = self.store.get_op(&at)? else { return Ok(None) };
            if op.lane.is_main() {
                let exact =
                    self.store.checkpoints().rev().find(|ck| ck.head == op.id).and_then(|ck| ck.git_commit);
                let filed = self
                    .lanes
                    .get(&lane)
                    .and_then(|at| self.store.checkpoint(at.record.base))
                    .and_then(|ck| ck.git_commit);
                return Ok(exact.or(filed));
            }
            match op.parents.first() {
                Some(parent) => at = *parent,
                None => return Ok(None),
            }
        }
    }

    fn export_git_with(&mut self, update_index: bool) -> Result<Vec<(String, String)>> {
        // The permanent lanes first: a checkpoint of the main lane may be a
        // merge with one of their commits.
        let permanent: Vec<LaneId> =
            self.lanes.values().filter(|at| at.record.is_permanent()).map(|at| at.record.id).collect();
        for lane in permanent {
            self.export_lane_git(lane)?;
        }
        // The files at the last checkpoint exported are remembered: the
        // next commit is made from what changed since, not from the log.
        let known = match (&self.exported, self.main.head) {
            (Some((exported, exported_tree)), Some(head)) => {
                Some(kito_git::Known { exported: *exported, exported_tree, head, tree: &self.main.tree })
            }
            _ => None,
        };
        let options = kito_git::ExportOptions { update_index, ..self.config.export_options() };
        let done = kito_git::export_known(&mut self.store, &self.root, &options, known.as_ref())?;
        self.index_stale = !update_index && (self.index_stale || !done.is_empty());
        self.exported = match (self.store.checkpoints().next_back(), self.main.head) {
            (Some(last), Some(head)) if last.head == head && last.git_commit.is_some() => {
                Some((last.id, self.main.tree.clone()))
            }
            _ => None,
        };
        Ok(done.into_iter().map(|e| (e.checkpoint.to_string(), hex20(&e.commit))).collect())
    }

    /// Imports the history of the git repository in `root` (empty store).
    pub fn import_git(&mut self) -> Result<kito_git::Imported> {
        let report = kito_git::import(&mut self.store, &self.root, &mut self.clock)?;
        self.main.tree = Tree::new();
        self.main.symbols = SymbolTable::new();
        self.main.head = None;
        self.main.ops = 0;
        self.main.stat.clear();
        self.load_state()?;
        // The imported history is text; the state it ends in gets the full analysis.
        self.analyze()?;
        self.save_state()?;
        Ok(report)
    }

    pub fn status(&self) -> Result<Status> {
        let checkpoints: Vec<&Checkpoint> = self.store.checkpoints().collect();
        let last = checkpoints.last();
        let ops_since_checkpoint = match last {
            Some(ck) if Some(ck.head) == self.main.head => 0,
            Some(ck) => {
                // Position of the checkpoint's op in the lane: search from the tail.
                let mut since = 0u64;
                let mut end = self.main.ops;
                'search: while end > 0 {
                    let start = end.saturating_sub(500);
                    let page = self.store.lane_ops(LaneId::MAIN, start, (end - start) as usize)?;
                    for op in page.iter().rev() {
                        if op.id == ck.head {
                            break 'search;
                        }
                        since += 1;
                    }
                    end = start;
                }
                since
            }
            None => self.main.ops,
        };
        Ok(Status {
            root: self.root.clone(),
            author: self.main.author.to_string(),
            identity: self.identity.clone(),
            head: self.main.head.map(|h| h.to_string()),
            ops: self.main.ops,
            files: self.main.tree.len(),
            symbols: self.main.symbols.len(),
            checkpoints: checkpoints.len(),
            last_checkpoint: last.map(|c| c.id.to_string()),
            unexported: checkpoints.iter().filter(|c| c.git_commit.is_none()).count(),
            protected: self.config.main.protected,
            checks: self.config.verify.commands.clone(),
            sandbox: self.sandbox_state().to_owned(),
            push: self.config.git.push.clone(),
            refused: self.refusals.0,
            last_refused: self.refusals.1.clone(),
            pushed: self.pushed.0.clone(),
            push_error: self.pushed.1.clone(),
            pull_waiting: self.pull_waiting().map(|(_, lane)| lane.to_string()),
            ops_since_checkpoint,
            counters: self.counters.clone(),
        })
    }

    /// The last `limit` changes of the main lane, most recent first, with the
    /// checkpoints interleaved. A change is a file-level op; the symbolic ops
    /// recorded with it (what the change meant) come right before it and do
    /// not count towards `limit`. With `path`, only what concerns that file.
    pub fn log(&self, limit: usize, path: Option<&Path>) -> Result<Vec<LogItem>> {
        self.log_lane(LaneId::MAIN, limit, path)
    }

    /// The state of a lane: the main one, or an open one.
    pub(crate) fn work(&self, lane: LaneId) -> Result<&Work> {
        match lane.is_main() {
            true => Ok(&self.main),
            false => Ok(&self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?.work),
        }
    }

    /// Like [`Repo::log`], for the ops a lane recorded itself.
    pub fn log_lane(&self, lane: LaneId, limit: usize, path: Option<&Path>) -> Result<Vec<LogItem>> {
        let work = self.work(lane)?;
        let mut by_head: HashMap<OpId, Vec<&Checkpoint>> = HashMap::new();
        for ck in self.store.checkpoints() {
            by_head.entry(ck.head).or_default().push(ck);
        }
        let mut reader = Backwards { store: &self.store, lane, end: work.ops, page: Vec::new() };
        let mut items = Vec::new();
        let mut shown = 0usize;
        // Symbolic ops waiting for the file change they belong to: in the
        // log they follow it, so going back in time they come first.
        let mut pending: Vec<Op> = Vec::new();
        // File-level ops met since the last symbolic one: the batch so far.
        let mut run = 0usize;
        let mut next = reader.next()?;
        while let Some(op) = next {
            if shown == limit {
                break;
            }
            next = reader.next()?;
            let symbolic = op.kind.symbol().is_some();
            if !symbolic && run == 0 {
                // A batch is recorded with one intent: symbolic ops with
                // another one are not about this change, nor about any other.
                let (mine, alone): (Vec<Op>, Vec<Op>) =
                    std::mem::take(&mut pending).into_iter().partition(|s| s.intent == op.intent);
                pending = mine;
                for symbolic in alone {
                    let file = self.file_then(&work.symbols, &symbolic, None);
                    if self.concerns(&symbolic, file.as_deref(), path) {
                        items.push(self.describe(&work.symbols, &symbolic, file, None));
                    }
                }
            }
            for ck in by_head.get(&op.id).into_iter().flatten().rev() {
                items.push(LogItem::Checkpoint {
                    id: ck.id.to_string(),
                    label: ck.label.clone(),
                    author: ck.created_by.name.clone(),
                    head: ck.head.to_string(),
                    git_commit: ck.git_commit.as_ref().map(hex20),
                });
            }
            if symbolic {
                if run > 0 {
                    // An older batch begins: what nobody took did not
                    // concern the file asked for.
                    pending.clear();
                    run = 0;
                }
                pending.push(op);
                continue;
            }
            run += 1;
            if path.is_some_and(|p| !op.kind.paths().iter().any(|q| q.as_path() == p)) {
                continue;
            }
            // The only file change of its batch: its symbols are in that file.
            let alone = run == 1 && next.as_ref().is_none_or(|older| older.kind.symbol().is_some());
            let carrier = op.kind.paths().last().map(|p| p.to_path_buf());
            for symbolic in std::mem::take(&mut pending) {
                let file =
                    self.file_then(&work.symbols, &symbolic, if alone { carrier.as_deref() } else { None });
                if self.concerns(&symbolic, file.as_deref(), path) {
                    items.push(self.describe(&work.symbols, &symbolic, file, Some(op.id)));
                } else {
                    pending.push(symbolic);
                }
            }
            items.push(self.describe(&work.symbols, &op, None, None));
            shown += 1;
        }
        // Symbolic ops with no file change before them (the first analysis
        // of files that were already there).
        if shown < limit && run == 0 {
            for symbolic in pending {
                let file = self.file_then(&work.symbols, &symbolic, None);
                if self.concerns(&symbolic, file.as_deref(), path) {
                    items.push(self.describe(&work.symbols, &symbolic, file, None));
                }
            }
        }
        Ok(items)
    }

    /// The file a symbol was in right after a symbolic op. `carrier`: the
    /// only file its batch changed, when there is only one.
    fn file_then(&self, symbols: &SymbolTable, op: &Op, carrier: Option<&Path>) -> Option<PathBuf> {
        match &op.kind {
            OpKind::AddSymbol { sym, .. } => return Some(sym.file.clone()),
            OpKind::Move { to, .. } => return Some(to.file.clone()),
            _ => {}
        }
        if let Some(carrier) = carrier {
            return Some(carrier.to_owned());
        }
        // Where its own ops had left it by then; failing that, where it is now.
        let id = op.kind.symbol()?;
        let mut file = None;
        for earlier in self.store.symbol_ops(id).ok()? {
            if earlier.at > op.at {
                break;
            }
            match earlier.kind {
                OpKind::AddSymbol { sym, .. } => file = Some(sym.file),
                OpKind::Move { to, .. } => file = Some(to.file),
                _ => {}
            }
        }
        file.or_else(|| symbols.get(id).map(|entry| entry.file.clone()))
    }

    /// `true` if a symbolic op belongs in the log of `path`.
    fn concerns(&self, op: &Op, file: Option<&Path>, path: Option<&Path>) -> bool {
        let Some(path) = path else { return true };
        file == Some(path) || matches!(&op.kind, OpKind::Move { from, .. } if from.file == path)
    }

    /// One line of the log. For a symbolic op, `file_then` is the file the
    /// symbol was in at the time and `change` the file change it belongs to.
    fn describe(
        &self,
        symbols: &SymbolTable,
        op: &Op,
        file_then: Option<PathBuf>,
        change: Option<OpId>,
    ) -> LogItem {
        let display = |p: &Path| p.to_string_lossy().into_owned();
        let mut symbol = None;
        let mut renamed_from = None;
        let (path, detail, stat) = match &op.kind {
            OpKind::FileAdd { path, mode, .. } => {
                let detail = match mode {
                    FileMode::Regular => None,
                    FileMode::Executable => Some("executable".to_owned()),
                    FileMode::Symlink => Some("symbolic link".to_owned()),
                };
                (Some(display(path)), detail, None)
            }
            OpKind::FileRemove { path } => (Some(display(path)), None, None),
            OpKind::FileRename { from, to } => {
                (Some(display(to)), Some(format!("from {}", display(from))), None)
            }
            OpKind::TextEdit { file, before, edits, .. } => {
                if edits.is_empty() {
                    (Some(display(file)), Some("binary or extensive change".to_owned()), None)
                } else {
                    let old = self.store.get_blob(before).ok().flatten().unwrap_or_default();
                    (Some(display(file)), None, Some(text::stat(&old, edits)))
                }
            }
            other => {
                // Symbolic ops: the file the symbol was in, and what
                // happened to it.
                let id = other.symbol();
                let file = file_then.or_else(|| self.file_then(symbols, op, None));
                let name = id.and_then(|id| {
                    symbols.qualified_name(id).or_else(|| semantic::last_known_name(&self.store, id))
                });
                let what = semantic::describe_symbol_op(&self.store, other);
                symbol = match other {
                    OpKind::AddSymbol { sym, .. } => Some(sym.name.clone()),
                    OpKind::Rename { from, to, .. } => {
                        renamed_from = Some(from.clone());
                        Some(to.clone())
                    }
                    _ => name.clone(),
                };
                let detail = match (name, what) {
                    (Some(name), Some(what))
                        if !matches!(other, OpKind::AddSymbol { .. } | OpKind::Rename { .. }) =>
                    {
                        Some(format!("`{name}` {what}"))
                    }
                    (_, what) => what,
                };
                (file.as_deref().map(display), detail, None)
            }
        };
        LogItem::Op {
            id: op.id.to_string(),
            at_ms: op.at.wall_ms,
            author: op.author.name.clone(),
            lane: op.lane.to_string(),
            op: op.kind.name().to_owned(),
            path,
            detail,
            lines_added: stat.map(|s| s.lines_added),
            lines_removed: stat.map(|s| s.lines_removed),
            intent: op.intent.and_then(|id| self.store.intent(&id)).map(|i| i.summary.clone()),
            change: change.map(|id| id.to_string()),
            symbol_id: op.kind.symbol().map(|id| id.to_string()),
            symbol,
            renamed_from,
        }
    }

    // ------------------------------------------------------------ rollback

    /// Ops of the main lane that satisfy `matches`, with their position
    /// (from 0), most recent first.
    fn positions_of(&self, matches: impl Fn(&Op) -> bool) -> Result<Vec<(u64, OpId)>> {
        let mut found = Vec::new();
        let mut end = self.main.ops;
        while end > 0 {
            let start = end.saturating_sub(500);
            let page = self.store.lane_ops(LaneId::MAIN, start, (end - start) as usize)?;
            for (i, op) in page.iter().enumerate().rev() {
                if matches(op) {
                    found.push((start + i as u64, op.id));
                }
            }
            end = start;
        }
        Ok(found)
    }

    /// Translates a point in the history into the number of ops that precede
    /// it (the state "after the first k ops") and into a readable description.
    ///
    /// Accepted forms: `ckpt#3`, `#3` or `3` (a checkpoint); `~2` (two ops
    /// ago); a prefix of at least 6 digits of an op's ID (the state right
    /// after that op).
    fn resolve_point(&self, spec: &str) -> Result<(u64, String)> {
        let spec = spec.trim();
        let invalid = |why: &str| Error::Other(format!("{spec:?}: {why}"));
        if let Some(back) = spec.strip_prefix('~') {
            let back: usize =
                back.parse().map_err(|_| invalid("~ must be followed by a number of file changes"))?;
            if back == 0 {
                return Ok((self.main.ops, "the current state".to_owned()));
            }
            // What counts are the ops that change a file: the symbolic ops
            // that follow them only say what the change meant.
            let file_ops = self.positions_of(|op| !op.kind.paths().is_empty())?;
            let Some((index, _)) = file_ops.get(back - 1) else {
                return Err(invalid(&format!("the history has only {}", file_changes(file_ops.len()))));
            };
            return Ok((*index, format!("{} ago", file_changes(back))));
        }
        let number = spec.strip_prefix("ckpt#").or_else(|| spec.strip_prefix('#')).unwrap_or(spec);
        let looks_like_checkpoint = number.len() != spec.len() || number.len() < 6;
        if looks_like_checkpoint {
            if let Ok(n) = number.parse::<u64>() {
                let ck = self
                    .store
                    .checkpoint(CheckpointId(n))
                    .ok_or_else(|| invalid("this checkpoint does not exist (see `kito log`)"))?;
                let head = ck.head;
                let found = self.positions_of(|op| op.id == head)?;
                let (index, _) =
                    found.first().ok_or_else(|| invalid("the checkpoint is not on the main lane"))?;
                let label = if ck.label.is_empty() { String::new() } else { format!(" ({})", ck.label) };
                return Ok((index + 1, format!("{}{label}", ck.id)));
            }
        }
        if spec.len() >= 6 && spec.bytes().all(|b| b.is_ascii_hexdigit()) {
            let prefix = spec.to_ascii_lowercase();
            let found = self.positions_of(|op| op.id.to_string().starts_with(&prefix))?;
            return match found.as_slice() {
                [(index, id)] => Ok((index + 1, format!("operation {}", id.hash().short()))),
                [] => Err(invalid("no operation with this ID (see `kito log`)")),
                _ => Err(invalid("several operations start like this: add a few more digits")),
            };
        }
        Err(invalid(
            "unrecognized point: use a checkpoint (ckpt#3), the first digits of an operation ID, or ~N",
        ))
    }

    /// The tree of the main lane after the first `k` ops.
    fn tree_at(&self, k: u64) -> Result<Tree> {
        let mut tree = Tree::new();
        let mut applied = 0u64;
        while applied < k {
            let page = self.store.lane_ops(LaneId::MAIN, applied, (k - applied).min(1000) as usize)?;
            if page.is_empty() {
                break;
            }
            for op in page {
                tree.apply(&op.kind)?;
                applied += 1;
            }
        }
        Ok(tree)
    }

    /// Brings the working directory back to the state it had at a point in
    /// the history (see [`Repo::resolve_point`]), for all files or only for
    /// those under `paths`.
    ///
    /// History is not rewritten: the current state is captured first, and
    /// going back is recorded as new ops. This is why a rollback can always
    /// be undone by returning to the point given in `undo`.
    pub fn rollback(&mut self, spec: &str, paths: &[PathBuf], dry_run: bool) -> Result<RollbackResult> {
        if !dry_run {
            self.main_is_open_to("a rollback")?;
        }
        self.scan()?;
        let (k, description) = self.resolve_point(spec)?;
        let target = self.tree_at(k)?;
        let selected = |path: &Path| paths.is_empty() || paths.iter().any(|p| path.starts_with(p));

        // What changes: files to rewrite (with the content they had then) and files to remove.
        let mut restore: Vec<(PathBuf, TreeEntry, &str)> = Vec::new();
        let mut remove: Vec<PathBuf> = Vec::new();
        for (path, entry) in target.iter().filter(|(p, _)| selected(p)) {
            match self.main.tree.get(path) {
                Some(current) if current == entry => {}
                Some(_) => restore.push((path.to_owned(), *entry, "restore")),
                None => restore.push((path.to_owned(), *entry, "recreate")),
            }
        }
        for (path, _) in self.main.tree.iter().filter(|(p, _)| selected(p)) {
            if !target.contains(path) {
                remove.push(path.to_owned());
            }
        }
        let mut changes: Vec<RollbackChange> = restore
            .iter()
            .map(|(p, _, action)| RollbackChange {
                path: p.display().to_string(),
                action: (*action).to_owned(),
            })
            .chain(
                remove
                    .iter()
                    .map(|p| RollbackChange { path: p.display().to_string(), action: "remove".to_owned() }),
            )
            .collect();
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        let undo = self.main.head.map(|h| h.hash().short());
        if dry_run || changes.is_empty() {
            return Ok(RollbackResult { target: description, changes, applied: false, ops: 0, undo: None });
        }

        // An untracked file (because it is ignored) in the place where a file
        // must be recreated: its content is not in the log and must not be
        // overwritten.
        for (path, _, _) in &restore {
            if !self.main.tree.contains(path) && fs::symlink_metadata(self.root.join(path)).is_ok() {
                return Err(Error::Other(format!(
                    "{}: exists but is not tracked; move it away before rolling back",
                    path.display()
                )));
            }
        }

        // Removals first (a file can give way to a directory and vice versa),
        // then the writes.
        for path in &remove {
            let abs = self.root.join(path);
            match fs::remove_file(&abs) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io(&abs)(e)),
            }
            // Directories left empty go away with their last file.
            let mut dir = path.parent();
            while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
                if fs::remove_dir(self.root.join(d)).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
        for (path, entry, _) in &restore {
            self.write_file(path, entry)?;
        }

        let intent = Intent::new(format!("rollback to {description}"), None, None)?;
        self.store.put_intent(&intent)?;
        let touched: Vec<PathBuf> = restore.into_iter().map(|(p, _, _)| p).chain(remove).collect();
        let ops = self.capture_as(touched, Some(intent.id))?;
        self.save_state()?;
        Ok(RollbackResult { target: description, changes, applied: true, ops: ops.len(), undo })
    }

    /// Writes into the main working directory a file with the given content and type.
    fn write_file(&self, rel: &Path, entry: &TreeEntry) -> Result<()> {
        self.write_file_in(&self.root, rel, entry)
    }

    /// Writes into a working directory a file with the given content and type.
    pub(crate) fn write_file_in(&self, root: &Path, rel: &Path, entry: &TreeEntry) -> Result<()> {
        let content = self.content_of(rel, entry)?;
        write_file(root, rel, entry.mode, &content)
    }

    fn content_of(&self, rel: &Path, entry: &TreeEntry) -> Result<Vec<u8>> {
        self.store.get_blob(&entry.blob)?.ok_or_else(|| {
            Error::Other(format!("{}: content {} is not in the store", rel.display(), entry.blob))
        })
    }

    /// The same for many files: the contents are read from the store one
    /// after the other, the files are written by several threads.
    pub(crate) fn write_files_in(&self, root: &Path, files: &[(PathBuf, TreeEntry)]) -> Result<()> {
        let mut ready = Vec::with_capacity(files.len());
        for (rel, entry) in files {
            ready.push((rel.as_path(), entry.mode, self.content_of(rel, entry)?));
        }
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).min(8);
        if ready.len() < 32 || workers < 2 {
            return ready.iter().try_for_each(|(rel, mode, content)| write_file(root, rel, *mode, content));
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> = ready
                .chunks(ready.len().div_ceil(workers))
                .map(|part| {
                    scope.spawn(move || {
                        part.iter().try_for_each(|(rel, mode, content)| write_file(root, rel, *mode, content))
                    })
                })
                .collect();
            handles.into_iter().try_for_each(|handle| handle.join().expect("a writer thread panicked"))
        })
    }

    /// Closes the repository, saving the state and leaving the indexes durable.
    pub fn close(mut self) -> Result<()> {
        // git is left as a clean mirror: an index that an export left
        // behind is brought up to the last commit.
        if let Err(e) = self.refresh_git_index() {
            eprintln!("kito: git index: {e}");
        }
        self.save_state()?;
        self.save_lanes()?;
        self.store.close()?;
        Ok(())
    }
}

/// Time taken by a capture, for the daemon's statistics.
pub fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}

/// Writes a file of a working directory: to a temporary file, then a
/// rename, so that readers never see it half-written.
pub(crate) fn write_file(root: &Path, rel: &Path, mode: FileMode, content: &[u8]) -> Result<()> {
    let abs = root.join(rel);
    if let Some(dir) = abs.parent() {
        fs::create_dir_all(dir).map_err(Error::io(dir))?;
    }
    #[cfg(unix)]
    if mode == FileMode::Symlink {
        use std::os::unix::ffi::OsStrExt;
        let _ = fs::remove_file(&abs);
        return std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(content), &abs)
            .map_err(Error::io(&abs));
    }
    // Elsewhere a link is a file that holds its target (ADR 0002).
    let name = abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = abs.with_file_name(format!(".{name}.kito-tmp"));
    fs::write(&tmp, content).map_err(Error::io(&tmp))?;
    #[cfg(unix)]
    {
        let mode = if mode == FileMode::Executable { 0o755 } else { 0o644 };
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(Error::io(&tmp))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    fs::rename(&tmp, &abs).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::io(&abs)(e)
    })
}
