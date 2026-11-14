//! Il motore di cattura: dalla cartella di lavoro alle op nel log.
//!
//! È codice sincrono e non sa nulla di watcher e socket: il daemon lo guida,
//! e i test lo usano direttamente.

use crate::config::{Config, CONFIG_FILE, DEFAULT_CONFIG};
use crate::error::{Error, Result};
use crate::ignore::IgnoreRules;
use kito_core::{codec, Author, BlobId, Checkpoint, FileMode, Hlc, HlcClock, LaneId, Op, OpId, OpKind, Tree};
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

/// Un file modificato da meno di così non entra nella cache degli stat: una
/// seconda scrittura nello stesso istante potrebbe non cambiarne dimensione
/// e data, e sfuggirebbe alla scansione.
const RACY_WINDOW: Duration = Duration::from_millis(100);

/// Dimensione, data di modifica e permessi di un file all'ultima cattura:
/// se non sono cambiati, la scansione non lo rilegge.
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

/// Stato della linea principale salvato su disco per ripartire in fretta:
/// si può sempre ricostruire riapplicando le op.
#[derive(Serialize, Deserialize)]
struct SavedState {
    /// Op della linea principale già applicate a `tree`.
    ops: u64,
    head: Option<OpId>,
    tree: Tree,
    stat: Vec<(PathBuf, FileStat)>,
    saved_ns: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    /// Batch di cattura che hanno prodotto almeno un'op.
    pub captures: u64,
    pub ops: u64,
    pub scans: u64,
    pub skipped_too_large: u64,
    pub skipped_not_utf8: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub root: PathBuf,
    pub author: String,
    pub head: Option<String>,
    pub ops: u64,
    pub files: usize,
    pub checkpoints: usize,
    pub last_checkpoint: Option<String>,
    /// Checkpoint non ancora esportati su git.
    pub unexported: usize,
    /// Op registrate dopo l'ultimo checkpoint.
    pub ops_since_checkpoint: u64,
    pub counters: Counters,
}

/// Una riga di `kito log`.
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
    /// Perché l'export su git non è riuscito, se è stato tentato.
    pub git_error: Option<String>,
}

pub struct Repo {
    root: PathBuf,
    store: Store,
    config: Config,
    author: Author,
    ignore: IgnoreRules,
    tree: Tree,
    head: Option<OpId>,
    /// Op della linea principale.
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

impl Repo {
    /// Crea `.kito/` in `root` con uno store vuoto e la configurazione di default.
    pub fn init(root: &Path) -> Result<Repo> {
        let store_dir = root.join(STORE_DIR);
        drop(Store::init(&store_dir)?);
        let config = store_dir.join(CONFIG_FILE);
        if !config.exists() {
            atomic_write(&config, DEFAULT_CONFIG.as_bytes()).map_err(Error::io(&config))?;
        }
        Repo::open(root)
    }

    /// Apre il repository in `root` e ricostruisce lo stato della linea principale.
    pub fn open(root: &Path) -> Result<Repo> {
        let root = root.canonicalize().map_err(Error::io(root))?;
        let store_dir = root.join(STORE_DIR);
        let store = Store::open(&store_dir)?;
        let config = Config::load(&store_dir).map_err(Error::Config)?;
        let ignore = IgnoreRules::new(&root, &config.capture.ignore);
        let author = config.author();
        let mut repo = Repo {
            root,
            store,
            config,
            author,
            ignore,
            tree: Tree::new(),
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

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// Stato della linea principale: percorso → blob.
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn head(&self) -> Option<OpId> {
        self.head
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(STORE_DIR).join("lanes").join("main").join("state")
    }

    /// Carica lo stato salvato, se corrisponde ancora al log, e riapplica le
    /// op successive. Senza stato salvato riapplica tutto dall'inizio.
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
            if matches_log {
                self.tree = saved.tree;
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
                self.head = Some(op.id);
                self.ops += 1;
            }
        }
        Ok(())
    }

    /// Salva lo stato della linea principale per la prossima apertura.
    pub fn save_state(&self) -> Result<()> {
        let saved = SavedState {
            ops: self.ops,
            head: self.head,
            tree: self.tree.clone(),
            stat: self.stat.iter().map(|(p, s)| (p.clone(), *s)).collect(),
            saved_ns: now_ns(),
        };
        let path = self.state_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(Error::io(dir))?;
        }
        atomic_write(&path, &codec::to_vec(&saved)?).map_err(Error::io(&path))
    }

    /// Percorso relativo alla radice, se `path` è dentro il repository.
    pub fn relative(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(&self.root).ok().map(Path::to_owned)
    }

    /// Scansione completa: confronta la cartella di lavoro con l'albero e
    /// cattura ogni differenza. È anche la barriera di `sync`: quando
    /// ritorna, tutto ciò che era su disco è nel log.
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
        // File dell'albero che la visita non ha incontrato: rimossi, oppure
        // finiti sotto una regola di ignore (e allora restano tracciati).
        for (path, _) in self.tree.iter() {
            if !seen.contains(path) {
                candidates.insert(path.to_owned());
            }
        }
        self.capture(candidates)
    }

    /// File e link non ignorati sotto `dir`, senza seguire i link.
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

    /// Cattura lo stato attuale dei percorsi dati (relativi alla radice) e
    /// registra le op che ne derivano. Una cartella vale per tutto ciò che
    /// contiene. Restituisce le op registrate, già confermate.
    pub fn capture(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> Result<Vec<Op>> {
        // 1. Dai percorsi segnalati ai singoli file da guardare.
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
                    // Un file sostituito da una cartella con lo stesso nome.
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

        // 2. Stato su disco di ogni file.
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
                    // Se il file sparisce tra lo stat e la lettura, è una rimozione.
                    fs::read(&abs).ok().map(|content| (content, mode, meta))
                }
                Ok(meta) if meta.file_type().is_symlink() => fs::read_link(&abs).ok().map(|target| {
                    (target.to_string_lossy().into_owned().into_bytes(), FileMode::Symlink, meta)
                }),
                // Cartelle, socket, fifo: non sono file dell'albero.
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

        // 3. Op, in un ordine applicabile all'albero, concatenate alla testa.
        let store = &self.store;
        let kinds = ops_for_changes(&self.tree, &changes, &mut |blob| store.get_blob(blob).ok().flatten());
        let mut ops = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let op = Op::new(self.head, LaneId::MAIN, self.author.clone(), self.clock.now(), kind, None)?;
            self.tree.apply(&op.kind)?;
            self.store.append_op(&op)?;
            self.head = Some(op.id);
            self.ops += 1;
            ops.push(op);
        }
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

    /// Crea un checkpoint sullo stato attuale della cartella di lavoro e, se
    /// configurato, lo esporta su git.
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

    /// Esporta su git i checkpoint non ancora esportati.
    pub fn export_git(&mut self) -> Result<Vec<(String, String)>> {
        let done = kito_git::export(&mut self.store, &self.root, &self.config.export_options())?;
        Ok(done.into_iter().map(|e| (e.checkpoint.to_string(), hex20(&e.commit))).collect())
    }

    /// Importa la storia del repository git in `root` (store vuoto).
    pub fn import_git(&mut self) -> Result<kito_git::Imported> {
        let report = kito_git::import(&mut self.store, &self.root, &mut self.clock)?;
        self.tree = Tree::new();
        self.head = None;
        self.ops = 0;
        self.stat.clear();
        self.load_state()?;
        self.save_state()?;
        Ok(report)
    }

    pub fn status(&self) -> Result<Status> {
        let checkpoints: Vec<&Checkpoint> = self.store.checkpoints().collect();
        let last = checkpoints.last();
        let ops_since_checkpoint = match last {
            Some(ck) if Some(ck.head) == self.head => 0,
            Some(ck) => {
                // Posizione dell'op del checkpoint nella lane: si cerca dalla coda.
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
            author: self.author.name.clone(),
            head: self.head.map(|h| h.to_string()),
            ops: self.ops,
            files: self.tree.len(),
            checkpoints: checkpoints.len(),
            last_checkpoint: last.map(|c| c.id.to_string()),
            unexported: checkpoints.iter().filter(|c| c.git_commit.is_none()).count(),
            ops_since_checkpoint,
            counters: self.counters.clone(),
        })
    }

    /// Le ultime `limit` op della linea principale, dalla più recente, con i
    /// checkpoint intercalati. Con `path`, solo le op che toccano quel file.
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
                    FileMode::Executable => Some("eseguibile".to_owned()),
                    FileMode::Symlink => Some("link simbolico".to_owned()),
                };
                (Some(display(path)), detail, None)
            }
            OpKind::FileRemove { path } => (Some(display(path)), None, None),
            OpKind::FileRename { from, to } => {
                (Some(display(to)), Some(format!("da {}", display(from))), None)
            }
            OpKind::TextEdit { file, before, edits, .. } => {
                if edits.is_empty() {
                    (Some(display(file)), Some("binario o modifica estesa".to_owned()), None)
                } else {
                    let old = self.store.get_blob(before).ok().flatten().unwrap_or_default();
                    (Some(display(file)), None, Some(text::stat(&old, edits)))
                }
            }
            other => (None, other.symbol().map(|s| format!("simbolo {s}")), None),
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
        }
    }

    /// Chiude il repository salvando lo stato e lasciando gli indici durevoli.
    pub fn close(self) -> Result<()> {
        self.save_state()?;
        self.store.close()?;
        Ok(())
    }
}

/// Tempo impiegato da una cattura, per le statistiche del daemon.
pub fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}
