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
    OpId, OpKind, SymbolTable, Tree, TreeEntry,
};
use kito_diff::fileops::{ops_for_changes, Snapshot};
use kito_diff::text;
use kito_store::fs::atomic_write;
use kito_store::{Store, STORE_DIR};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A file modified less than this long ago does not enter the stat cache: a
/// second write in the same instant might not change its size and
/// modification time, and would escape the scan.
const RACY_WINDOW: Duration = Duration::from_millis(100);

/// Layout version of the saved state of the main lane.
const STATE_VERSION: u32 = 2;

/// Size, modification time and permissions of a file at the last capture:
/// if they have not changed, the scan does not reread it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileStat {
    size: u64,
    mtime_ns: i64,
    mode: u32,
}

impl FileStat {
    fn of(meta: &fs::Metadata) -> FileStat {
        FileStat {
            size: meta.len(),
            mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            mode: meta.mode(),
        }
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

pub struct Repo {
    root: PathBuf,
    store: Store,
    config: Config,
    identity: Identity,
    author: Author,
    ignore: IgnoreRules,
    tree: Tree,
    symbols: SymbolTable,
    /// For files whose current content has syntax errors: the last content
    /// that parsed, to compare the next clean save with. A hint, not state:
    /// without it only the names of the symbols are compared.
    last_clean: HashMap<PathBuf, BlobId>,
    head: Option<OpId>,
    /// Ops of the main lane.
    ops: u64,
    clock: HlcClock,
    stat: HashMap<PathBuf, FileStat>,
    counters: Counters,
}

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

    /// Opens the repository in `root` and rebuilds the state of the main lane.
    pub fn open(root: &Path) -> Result<Repo> {
        let root = root.canonicalize().map_err(Error::io(root))?;
        let store_dir = root.join(STORE_DIR);
        let store = Store::open(&store_dir)?;
        let config = Config::load(&store_dir).map_err(Error::Config)?;
        let ignore = IgnoreRules::new(&root, &config.capture.ignore);
        let identity = identity::resolve(&config, &root);
        let author = identity.author();
        let mut repo = Repo {
            root,
            store,
            config,
            identity,
            author,
            ignore,
            tree: Tree::new(),
            symbols: SymbolTable::new(),
            last_clean: HashMap::new(),
            head: None,
            ops: 0,
            clock: HlcClock::new(),
            stat: HashMap::new(),
            counters: Counters::default(),
        };
        repo.load_state()?;
        if let Some(head) = repo.head {
            let last = repo.store.get_op(&head)?.map_or(Hlc::ZERO, |op| op.at);
            repo.clock = HlcClock::starting_after(last);
        }
        Ok(repo)
    }

    pub fn root(&self) -> &Path {
        &self.root
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
        self.ignore = IgnoreRules::new(&self.root, &self.config.capture.ignore);
        self.identity = identity::resolve(&self.config, &self.root);
        self.author = self.identity.author();
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
        &self.tree
    }

    pub fn head(&self) -> Option<OpId> {
        self.head
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
                self.tree = saved.tree;
                self.symbols = saved.symbols;
                self.last_clean = saved.last_clean.into_iter().collect();
                self.head = saved.head;
                self.ops = saved.ops;
                let racy_after = saved.saved_ns - RACY_WINDOW.as_nanos() as i64;
                self.stat = saved.stat.into_iter().filter(|(_, s)| s.mtime_ns < racy_after).collect();
            }
        }
        while self.ops < total {
            let page = self.store.lane_ops(LaneId::MAIN, self.ops, 1000)?;
            if page.is_empty() {
                break;
            }
            for op in page {
                for path in op.kind.paths() {
                    self.stat.remove(path);
                }
                self.tree.apply(&op.kind)?;
                self.symbols.apply(&op.kind)?;
                self.head = Some(op.id);
                self.ops += 1;
            }
        }
        Ok(())
    }

    /// Saves the state of the main lane for the next open.
    pub fn save_state(&self) -> Result<()> {
        let saved = SavedState {
            version: STATE_VERSION,
            ops: self.ops,
            head: self.head,
            tree: self.tree.clone(),
            symbols: self.symbols.clone(),
            last_clean: self.last_clean.iter().map(|(p, b)| (p.clone(), *b)).collect(),
            stat: self.stat.iter().map(|(p, s)| (p.clone(), *s)).collect(),
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
        path.strip_prefix(&self.root).ok().map(Path::to_owned)
    }

    /// Full scan: compares the working directory with the tree and captures
    /// every difference. It is also the barrier of `sync`: when it returns,
    /// everything that was on disk is in the log.
    pub fn scan(&mut self) -> Result<Vec<Op>> {
        self.counters.scans += 1;
        let mut found = Vec::new();
        self.walk(Path::new(""), &mut found);
        let mut candidates = BTreeSet::new();
        let mut seen = HashSet::new();
        for (rel, meta) in found {
            let unchanged = self.tree.contains(&rel) && self.stat.get(&rel) == Some(&FileStat::of(&meta));
            if !unchanged {
                candidates.insert(rel.clone());
            }
            seen.insert(rel);
        }
        // Files of the tree that the walk did not come across: removed, or
        // now covered by an ignore rule (in which case they stay tracked).
        for (path, _) in self.tree.iter() {
            if !seen.contains(path) {
                candidates.insert(path.to_owned());
            }
        }
        self.capture(candidates)
    }

    /// Non-ignored files and links under `dir`, without following links.
    fn walk(&self, dir: &Path, out: &mut Vec<(PathBuf, fs::Metadata)>) {
        let Ok(entries) = fs::read_dir(self.root.join(dir)) else { return };
        for entry in entries.flatten() {
            let rel = dir.join(entry.file_name());
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if !self.ignore.is_ignored(&rel, true) {
                    self.walk(&rel, out);
                }
            } else if (kind.is_file() || kind.is_symlink()) && !self.ignore.is_ignored(&rel, false) {
                if let Ok(meta) = entry.metadata() {
                    out.push((rel, meta));
                }
            }
        }
    }

    /// Captures the current state of the given paths (relative to the root)
    /// and records the resulting ops. A directory stands for everything it
    /// contains. Returns the recorded ops, already confirmed.
    pub fn capture(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> Result<Vec<Op>> {
        self.capture_as(paths, None)
    }

    /// Like [`Repo::capture`], declaring the "why" of the recorded ops.
    fn capture_as(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        intent: Option<IntentId>,
    ) -> Result<Vec<Op>> {
        // 1. From the reported paths to the individual files to look at.
        let mut files: BTreeSet<PathBuf> = BTreeSet::new();
        for rel in paths {
            if rel.file_name().is_some_and(|n| n == ".gitignore" || n == ".kitoignore") {
                self.ignore.invalidate();
            }
            let tracked_below: Vec<PathBuf> = self.tree.under(&rel).map(|(p, _)| p.to_owned()).collect();
            match fs::symlink_metadata(self.root.join(&rel)) {
                Ok(meta) if meta.is_dir() => {
                    if !self.ignore.is_ignored(&rel, true) {
                        let mut found = Vec::new();
                        self.walk(&rel, &mut found);
                        files.extend(found.into_iter().map(|(p, _)| p));
                    }
                    files.extend(tracked_below);
                    // A file replaced by a directory with the same name.
                    if self.tree.contains(&rel) {
                        files.insert(rel);
                    }
                }
                _ => {
                    files.extend(tracked_below);
                    if self.tree.contains(&rel) || !self.ignore.is_ignored(&rel, false) {
                        files.insert(rel);
                    }
                }
            }
        }

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
            let abs = self.root.join(&rel);
            let on_disk = match fs::symlink_metadata(&abs) {
                Ok(meta) if meta.is_file() => {
                    if meta.len() > self.config.capture.max_file_bytes {
                        self.counters.skipped_too_large += 1;
                        continue;
                    }
                    let mode = if meta.permissions().mode() & 0o111 != 0 {
                        FileMode::Executable
                    } else {
                        FileMode::Regular
                    };
                    // If the file disappears between the stat and the read, it is a removal.
                    fs::read(&abs).ok().map(|content| (content, mode, meta))
                }
                Ok(meta) if meta.file_type().is_symlink() => fs::read_link(&abs).ok().map(|target| {
                    (target.to_string_lossy().into_owned().into_bytes(), FileMode::Symlink, meta)
                }),
                // Directories, sockets, fifos: they are not files of the tree.
                _ => None,
            };
            match on_disk {
                Some((content, mode, meta)) => {
                    let blob = BlobId::of(&content);
                    let same = self.tree.get(&rel).is_some_and(|e| e.blob == blob && e.mode == mode);
                    if !same {
                        self.store.put_blob(&content)?;
                        changes.insert(rel.clone(), Some(Snapshot { blob, mode }));
                    }
                    let fresh = meta
                        .modified()
                        .ok()
                        .and_then(|m| m.elapsed().ok())
                        .is_none_or(|age| age < RACY_WINDOW);
                    stats.push((rel, (!fresh).then(|| FileStat::of(&meta))));
                }
                None => {
                    if self.tree.contains(&rel) {
                        changes.insert(rel.clone(), None);
                    }
                    stats.push((rel, None));
                }
            }
        }

        // 3. Ops, in an order that can be applied to the tree, chained to the head.
        let store = &self.store;
        let file_ops = ops_for_changes(&self.tree, &changes, &mut |blob| store.get_blob(blob).ok().flatten());
        // The file-level ops carry the new contents; what the change means
        // for the symbols follows.
        let symbolic = semantic::symbol_ops(
            &self.store,
            &self.tree,
            &self.symbols,
            LaneId::MAIN,
            &file_ops,
            &mut self.last_clean,
        )?;
        for blob in &symbolic.blobs {
            self.store.put_blob(blob)?;
        }
        let ops = self.record(file_ops.into_iter().chain(symbolic.ops), intent)?;
        if !ops.is_empty() {
            self.store.commit()?;
            self.counters.captures += 1;
            self.counters.ops += ops.len() as u64;
        }
        for (rel, stat) in stats {
            match stat {
                Some(stat) => self.stat.insert(rel, stat),
                None => self.stat.remove(&rel),
            };
        }
        Ok(ops)
    }

    /// Appends ops to the main lane, chained to the head, and applies them
    /// to the state. They are confirmed by the caller's commit.
    fn record(
        &mut self,
        kinds: impl IntoIterator<Item = OpKind>,
        intent: Option<IntentId>,
    ) -> Result<Vec<Op>> {
        let mut ops = Vec::new();
        for kind in kinds {
            let op = Op::new(self.head, LaneId::MAIN, self.author.clone(), self.clock.now(), kind, intent)?;
            self.tree.apply(&op.kind)?;
            self.symbols.apply(&op.kind)?;
            self.store.append_op(&op)?;
            self.head = Some(op.id);
            self.ops += 1;
            ops.push(op);
        }
        Ok(ops)
    }

    /// Analyzes the files the history knows only as text (imported from git,
    /// or captured before symbols existed) and records their symbols. Does
    /// nothing once every parseable file has been analyzed.
    pub fn analyze(&mut self) -> Result<usize> {
        let found = semantic::analyze_unindexed(&self.store, &self.tree, &self.symbols, LaneId::MAIN)?;
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
        &self.symbols
    }

    /// The history of the symbol called `name` in `file`, across renames
    /// and moves.
    pub fn why(&self, file: &Path, name: &str) -> Result<Vec<Why>> {
        semantic::why(&self.store, &self.symbols, file, name)
    }

    /// Creates a checkpoint on the current state of the working directory
    /// and, if configured, exports it to git.
    pub fn checkpoint(&mut self, label: &str) -> Result<CheckpointResult> {
        self.scan()?;
        let head = self.head.ok_or(Error::NothingToCheckpoint)?;
        let checkpoint = Checkpoint {
            id: self.store.next_checkpoint_id(),
            head,
            label: label.trim().to_owned(),
            created_by: self.author.clone(),
            verification: None,
            git_commit: None,
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
    pub fn export_git(&mut self) -> Result<Vec<(String, String)>> {
        let done = kito_git::export(&mut self.store, &self.root, &self.config.export_options())?;
        Ok(done.into_iter().map(|e| (e.checkpoint.to_string(), hex20(&e.commit))).collect())
    }

    /// Imports the history of the git repository in `root` (empty store).
    pub fn import_git(&mut self) -> Result<kito_git::Imported> {
        let report = kito_git::import(&mut self.store, &self.root, &mut self.clock)?;
        self.tree = Tree::new();
        self.symbols = SymbolTable::new();
        self.head = None;
        self.ops = 0;
        self.stat.clear();
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
            Some(ck) if Some(ck.head) == self.head => 0,
            Some(ck) => {
                // Position of the checkpoint's op in the lane: search from the tail.
                let mut since = 0u64;
                let mut end = self.ops;
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
            None => self.ops,
        };
        Ok(Status {
            root: self.root.clone(),
            author: self.author.to_string(),
            identity: self.identity.clone(),
            head: self.head.map(|h| h.to_string()),
            ops: self.ops,
            files: self.tree.len(),
            symbols: self.symbols.len(),
            checkpoints: checkpoints.len(),
            last_checkpoint: last.map(|c| c.id.to_string()),
            unexported: checkpoints.iter().filter(|c| c.git_commit.is_none()).count(),
            ops_since_checkpoint,
            counters: self.counters.clone(),
        })
    }

    /// The last `limit` ops of the main lane, most recent first, with the
    /// checkpoints interleaved. With `path`, only the ops that touch that file.
    pub fn log(&self, limit: usize, path: Option<&Path>) -> Result<Vec<LogItem>> {
        let mut by_head: HashMap<OpId, Vec<&Checkpoint>> = HashMap::new();
        for ck in self.store.checkpoints() {
            by_head.entry(ck.head).or_default().push(ck);
        }
        let mut items = Vec::new();
        let mut shown = 0usize;
        let mut end = self.ops;
        while end > 0 && shown < limit {
            let start = end.saturating_sub(500);
            let page = self.store.lane_ops(LaneId::MAIN, start, (end - start) as usize)?;
            for op in page.iter().rev() {
                if let Some(checkpoints) = by_head.get(&op.id) {
                    for ck in checkpoints.iter().rev() {
                        items.push(LogItem::Checkpoint {
                            id: ck.id.to_string(),
                            label: ck.label.clone(),
                            author: ck.created_by.name.clone(),
                            head: ck.head.to_string(),
                            git_commit: ck.git_commit.as_ref().map(hex20),
                        });
                    }
                }
                if path.is_some_and(|p| !op.kind.paths().iter().any(|q| q.as_path() == p)) {
                    continue;
                }
                items.push(self.describe(op));
                shown += 1;
                if shown == limit {
                    break;
                }
            }
            end = start;
        }
        Ok(items)
    }

    fn describe(&self, op: &Op) -> LogItem {
        let display = |p: &Path| p.to_string_lossy().into_owned();
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
                // Symbolic ops: the file the symbol is in now (or was last),
                // and what happened to it.
                let id = other.symbol();
                let file = match other {
                    OpKind::AddSymbol { sym, .. } => Some(sym.file.clone()),
                    OpKind::Move { to, .. } => Some(to.file.clone()),
                    _ => id.and_then(|id| self.symbols.get(id)).map(|entry| entry.file.clone()),
                };
                let name = id.and_then(|id| {
                    self.symbols.qualified_name(id).or_else(|| semantic::last_known_name(&self.store, id))
                });
                let what = semantic::describe_symbol_op(&self.store, other);
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
        }
    }

    // ------------------------------------------------------------ rollback

    /// Ops of the main lane that satisfy `matches`, with their position
    /// (from 0), most recent first.
    fn positions_of(&self, matches: impl Fn(&Op) -> bool) -> Result<Vec<(u64, OpId)>> {
        let mut found = Vec::new();
        let mut end = self.ops;
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
                return Ok((self.ops, "the current state".to_owned()));
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
        self.scan()?;
        let (k, description) = self.resolve_point(spec)?;
        let target = self.tree_at(k)?;
        let selected = |path: &Path| paths.is_empty() || paths.iter().any(|p| path.starts_with(p));

        // What changes: files to rewrite (with the content they had then) and files to remove.
        let mut restore: Vec<(PathBuf, TreeEntry, &str)> = Vec::new();
        let mut remove: Vec<PathBuf> = Vec::new();
        for (path, entry) in target.iter().filter(|(p, _)| selected(p)) {
            match self.tree.get(path) {
                Some(current) if current == entry => {}
                Some(_) => restore.push((path.to_owned(), *entry, "restore")),
                None => restore.push((path.to_owned(), *entry, "recreate")),
            }
        }
        for (path, _) in self.tree.iter().filter(|(p, _)| selected(p)) {
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
        let undo = self.head.map(|h| h.hash().short());
        if dry_run || changes.is_empty() {
            return Ok(RollbackResult { target: description, changes, applied: false, ops: 0, undo: None });
        }

        // An untracked file (because it is ignored) in the place where a file
        // must be recreated: its content is not in the log and must not be
        // overwritten.
        for (path, _, _) in &restore {
            if !self.tree.contains(path) && fs::symlink_metadata(self.root.join(path)).is_ok() {
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

    /// Writes into the working directory a file with the given content and type.
    fn write_file(&self, rel: &Path, entry: &TreeEntry) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let abs = self.root.join(rel);
        if let Some(dir) = abs.parent() {
            fs::create_dir_all(dir).map_err(Error::io(dir))?;
        }
        let content = self.store.get_blob(&entry.blob)?.ok_or_else(|| {
            Error::Other(format!("{}: content {} is not in the store", rel.display(), entry.blob))
        })?;
        if entry.mode == FileMode::Symlink {
            let _ = fs::remove_file(&abs);
            return std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&content), &abs)
                .map_err(Error::io(&abs));
        }
        // Write to a temporary file, then rename: readers never see a half-written file.
        let name = abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = abs.with_file_name(format!(".{name}.kito-tmp"));
        fs::write(&tmp, &content).map_err(Error::io(&tmp))?;
        let mode = if entry.mode == FileMode::Executable { 0o755 } else { 0o644 };
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(Error::io(&tmp))?;
        fs::rename(&tmp, &abs).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            Error::io(&abs)(e)
        })
    }

    /// Closes the repository, saving the state and leaving the indexes durable.
    pub fn close(self) -> Result<()> {
        self.save_state()?;
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
