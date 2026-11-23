//! What was (ADR 0024): the checkpoints of a lane, like commits, and what
//! each one changed; the files as they were at any of them; the lanes that
//! are closed, as they ended.
//!
//! The history behind a checkpoint never changes. What is worked out from
//! it is kept in memory while the daemon runs, and only what was recorded
//! since is read again.

use crate::error::{Error, Result};
use crate::repo::Repo;
use crate::review::{FileDiff, Said, Side};
use kito_core::{
    Checkpoint, CheckpointId, FileMode, Integration, Lane, LaneId, Op, OpId, OpKind, SymbolTable, Tree,
    TreeEntry,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// How many states rebuilt from the history are kept: the only whole
/// states kept, whatever the number of checkpoints.
pub const STATES_KEPT: usize = 8;

/// How many checkpoints `tree` looks through, newest first, for the last
/// change of each entry.
pub const LAST_LOOKED: usize = 1000;

/// How many ops `diff` follows back from one point to find the other,
/// when the two are not checkpoints of the main lane.
pub const BETWEEN_FOLLOWED: usize = 5_000;

/// How many checkpoints `checkpoints` gives, unless asked for another
/// number, and the most it gives.
pub const LISTED: usize = 50;
pub const LISTED_AT_MOST: usize = 200;

/// The files and the symbols at one point of the history.
pub(crate) type State = (Tree, SymbolTable);

/// What is kept in memory of the history.
#[derive(Default)]
pub(crate) struct Memory {
    /// The states last rebuilt, the most recent last.
    states: Mutex<VecDeque<(OpId, State)>>,
    /// For the main lane and each permanent lane: what its checkpoints
    /// changed.
    indexes: Mutex<HashMap<LaneId, Index>>,
    /// For each checkpoint's head on the main lane: how many of its ops
    /// come up to it, that op included.
    positions: Mutex<HashMap<OpId, u64>>,
    /// For each closed lane: how many ops it recorded, and the files they
    /// touched.
    closed: Mutex<HashMap<LaneId, (u64, Vec<String>)>>,
    /// The integrations of lanes from before Kito 0.41, as the history
    /// says them.
    derived: Mutex<Option<BTreeMap<LaneId, Vec<Integration>>>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a checkpoint changed in one file.
#[derive(Clone, Debug)]
struct Changed {
    path: PathBuf,
    /// The name it had, for a file renamed.
    from: Option<PathBuf>,
    before: Option<TreeEntry>,
    after: Option<TreeEntry>,
    /// Lines added and removed, once counted.
    lines: Option<(usize, usize)>,
}

impl Changed {
    /// Whether it is about `path`, or about something under it.
    fn touches(&self, path: &Path) -> bool {
        self.path.starts_with(path) || self.from.as_ref().is_some_and(|from| from.starts_with(path))
    }
}

/// A lane's history replayed once, up to its last op: what each of its
/// checkpoints changed. Its files as they are at the last op replayed are
/// the only state it keeps.
#[derive(Default)]
struct Index {
    started: bool,
    /// Ops of the lane replayed.
    ops: u64,
    last: Option<OpId>,
    /// For a permanent lane: the op of the lane below it that it started from.
    start: Option<OpId>,
    tree: Tree,
    /// Since the last checkpoint: each file touched, as it was then.
    touched: BTreeMap<PathBuf, Option<TreeEntry>>,
    /// Since the last checkpoint: each file renamed, with its first name.
    renamed: BTreeMap<PathBuf, PathBuf>,
    changes: HashMap<CheckpointId, Vec<Changed>>,
}

impl Index {
    /// Notes a file about to change.
    fn touch(&mut self, path: &Path) {
        if !self.touched.contains_key(path) {
            let then = self.tree.get(path).copied();
            self.touched.insert(path.to_owned(), then);
        }
    }

    /// What changed since the last checkpoint: the next one is made here.
    fn close(&mut self) -> Vec<Changed> {
        let touched = std::mem::take(&mut self.touched);
        let renamed = std::mem::take(&mut self.renamed);
        let now = |path: &Path| self.tree.get(path).copied();
        let then = |path: &Path| touched.get(path).copied().flatten();
        let mut found = Vec::new();
        let mut told: BTreeSet<PathBuf> = BTreeSet::new();
        for (to, from) in renamed {
            if to != from && then(&from).is_some() && now(&from).is_none() && then(&to).is_none() {
                if let Some(after) = now(&to) {
                    found.push(Changed {
                        path: to.clone(),
                        before: then(&from),
                        after: Some(after),
                        from: Some(from.clone()),
                        lines: None,
                    });
                    told.extend([to, from]);
                }
            }
        }
        for (path, before) in &touched {
            let after = now(path);
            if !told.contains(path) && *before != after {
                found.push(Changed { path: path.clone(), from: None, before: *before, after, lines: None });
            }
        }
        found.sort_by(|a, b| a.path.cmp(&b.path));
        found
    }
}

/// What differs between two trees, file by file (no renames: nothing says
/// which file became which).
fn changes_between(before: &Tree, after: &Tree) -> Vec<Changed> {
    before
        .differences(after)
        .map(|path| Changed {
            path: path.to_owned(),
            from: None,
            before: before.get(path).copied(),
            after: after.get(path).copied(),
            lines: None,
        })
        .collect()
}

/// How a checkpoint's checks ended: what `verification` keeps. Whether
/// each command passed is not kept, only whether they all did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checks {
    pub commands: Vec<String>,
    pub passed: bool,
    pub duration_ms: u64,
}

/// A file a checkpoint changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    /// `added`, `modified`, `removed` or `renamed`.
    pub status: String,
    /// The name it had, when renamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub lines_added: usize,
    pub lines_removed: usize,
    /// The IDs of its contents before and after: `get_blobs` gives them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// A checkpoint, as a history of commits shows one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointInfo {
    /// `ckpt#4`.
    pub id: String,
    pub label: String,
    /// When it was made. For a checkpoint from before Kito 0.41: when the
    /// last op it holds was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
    /// Who made it: for an integration, the lane's agent.
    pub by: String,
    /// The permanent lane it is a checkpoint of; absent for the main lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// For an integration: the lane it came from (`lane/3`), its agent,
    /// whom it was for, the assistant, its task and its intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<kito_core::Assistant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// How many files it changed, and the lines.
    pub files: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    #[serde(default)]
    pub reviewed_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Checks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// The checkpoint before it on the same lane; absent for the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Its files, when one checkpoint is asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<Vec<ChangedFile>>,
}

/// A page of checkpoints, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointPage {
    pub checkpoints: Vec<CheckpointInfo>,
    /// There are older ones: ask with `before` the last one given.
    pub more: bool,
}

/// What differs between two points of the history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    /// `ckpt#3`, `main`, `lane/3`; absent: from nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
    pub files: Vec<FileDiff>,
}

/// The last checkpoint that changed an entry of a directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Last {
    pub checkpoint: String,
    /// Its label, or the intent of the lane it came from.
    pub why: String,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
}

/// One entry of a directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    /// `dir` or `file`.
    pub kind: String,
    /// For a file, the ID of its content: `get_blobs` gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub symlink: bool,
    /// Absent when it was not found among the last [`LAST_LOOKED`]
    /// checkpoints at or before the point asked about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Last>,
}

/// What a directory holds at one point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub at: String,
    /// The directory, from the repository's root: `` for the root.
    pub path: String,
    /// Directories first, then files, each by name.
    pub entries: Vec<DirEntry>,
    /// The name of its README, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readme: Option<String>,
}

/// A point of the history: a checkpoint, the main lane as it is, or a
/// lane (as it is, or as it ended).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    Checkpoint(CheckpointId),
    Main,
    Lane(LaneId),
}

/// A path asked for, as the tree has paths: no `./`, no `/` at either end.
fn path_asked(path: &str) -> PathBuf {
    let path = kito_core::paths::portable_text(path.trim());
    let path = path.trim_start_matches("./").trim_matches('/');
    match path {
        "." => PathBuf::new(),
        path => PathBuf::from(path),
    }
}

fn shown(path: &Path) -> String {
    kito_core::paths::portable_text(&path.display().to_string())
}

/// `README.md` first, then `README`, then the others by name.
fn readme_of<'a>(names: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut found: Vec<&str> = names
        .filter(|name| {
            let lower = name.to_lowercase();
            lower == "readme" || lower.starts_with("readme.")
        })
        .collect();
    found.sort_by_key(|name| {
        let lower = name.to_lowercase();
        (lower != "readme.md", lower != "readme", lower, name.to_string())
    });
    found.first().map(|name| (*name).to_owned())
}

impl Repo {
    /// The state right after an op, as [`Repo::state_at`] rebuilds it,
    /// kept for the next question.
    pub(crate) fn state_of(&self, head: OpId) -> Result<State> {
        if let Some(state) = self.kept_state(head) {
            return Ok(state);
        }
        let state = self.state_at(head)?;
        let mut kept = locked(&self.history.states);
        kept.push_back((head, state.clone()));
        while kept.len() > STATES_KEPT {
            kept.pop_front();
        }
        Ok(state)
    }

    /// A state already rebuilt, among the last ones asked for.
    pub(crate) fn kept_state(&self, head: OpId) -> Option<State> {
        let mut kept = locked(&self.history.states);
        let at = kept.iter().position(|(op, _)| *op == head)?;
        let found = kept.remove(at)?;
        let state = found.1.clone();
        kept.push_back(found);
        Some(state)
    }

    /// The state of the main lane right after its op `head`: from the
    /// nearest state kept before it when its place is known (a
    /// checkpoint's head), from the first op otherwise.
    pub(crate) fn main_state(&self, head: OpId) -> Result<State> {
        let target = locked(&self.history.positions).get(&head).copied();
        let Some(target) = target else { return self.main_state_at(head) };
        // The one to start from is found under the locks; only it is copied.
        let nearest = {
            let kept = locked(&self.history.states);
            let positions = locked(&self.history.positions);
            kept.iter()
                .filter_map(|(op, state)| {
                    positions.get(op).filter(|at| **at <= target).map(|at| (*at, state))
                })
                .max_by_key(|(at, _)| *at)
                .map(|(at, state)| (at, state.clone()))
        };
        let (mut at, (mut tree, mut symbols)) = nearest.unwrap_or_default();
        while at < target {
            let page = self.store.lane_ops(LaneId::MAIN, at, (target - at).min(1000) as usize)?;
            if page.is_empty() {
                return self.main_state_at(head);
            }
            for op in &page {
                tree.apply(&op.kind)?;
                symbols.apply(&op.kind)?;
            }
            at += page.len() as u64;
        }
        Ok((tree, symbols))
    }

    /// Forgets what was worked out of the history: it was replaced.
    pub(crate) fn forget_history(&self) {
        *locked(&self.history.states) = VecDeque::new();
        *locked(&self.history.indexes) = HashMap::new();
        *locked(&self.history.positions) = HashMap::new();
        *locked(&self.history.closed) = HashMap::new();
        *locked(&self.history.derived) = None;
    }

    /// Runs `f` on the index of a lane's checkpoints, brought up to its
    /// last op. The index is taken out meanwhile: rebuilding a state may
    /// look at the others.
    fn with_index<T>(&self, lane: LaneId, f: impl FnOnce(&Repo, &mut Index) -> Result<T>) -> Result<T> {
        let mut index = locked(&self.history.indexes).remove(&lane).unwrap_or_default();
        let result = self.bring_up(lane, &mut index).and_then(|()| f(self, &mut index));
        locked(&self.history.indexes).insert(lane, index);
        result
    }

    /// Replays what the lane recorded since the index last looked.
    fn bring_up(&self, lane: LaneId, index: &mut Index) -> Result<()> {
        let total = self.store.lane_len(lane)?;
        if total < index.ops {
            *index = Index::default();
        }
        if !index.started {
            index.started = true;
            // A permanent lane starts from the lane below it as it was.
            if !lane.is_main() {
                let first = self.store.lane_ops(lane, 0, 1)?;
                let start = match first.first() {
                    Some(op) => op.parents.first().copied(),
                    None => self.store.lane_record(lane).map(|record| record.base_head),
                };
                if let Some(start) = start {
                    index.tree = self.state_of(start)?.0;
                    (index.last, index.start) = (Some(start), Some(start));
                }
            }
        }
        let mut heads: HashMap<OpId, Vec<CheckpointId>> = HashMap::new();
        for ck in self.store.lane_checkpoints(lane) {
            if !index.changes.contains_key(&ck.id) {
                heads.entry(ck.head).or_default().push(ck.id);
            }
        }
        // Checkpoints made since on the last op replayed.
        if let Some(ids) = index.last.and_then(|last| heads.remove(&last)) {
            index.checkpoint_here(&ids);
            self.note_place(lane, index);
        }
        while index.ops < total {
            let page = self.store.lane_ops(lane, index.ops, 1000)?;
            if page.is_empty() {
                break;
            }
            for op in page {
                // Brought up to the lane below: its state starts from there.
                if let Some(parent) =
                    op.parents.first().filter(|p| !lane.is_main() && Some(**p) != index.last)
                {
                    let tree = self.state_of(*parent)?.0;
                    let moved: Vec<PathBuf> = index.tree.differences(&tree).map(Path::to_owned).collect();
                    moved.iter().for_each(|path| index.touch(path));
                    index.tree = tree;
                }
                for path in op.kind.paths() {
                    index.touch(path);
                }
                if let OpKind::FileRename { from, to } = &op.kind {
                    let first = index.renamed.remove(from).unwrap_or_else(|| from.clone());
                    index.renamed.insert(to.clone(), first);
                }
                index.tree.apply(&op.kind)?;
                index.last = Some(op.id);
                index.ops += 1;
                if let Some(ids) = heads.remove(&op.id) {
                    index.checkpoint_here(&ids);
                    self.note_place(lane, index);
                }
            }
        }
        Ok(())
    }

    /// Notes where on the main lane the op last replayed is: a state
    /// asked for there starts from the nearest one kept.
    fn note_place(&self, lane: LaneId, index: &Index) {
        if let (true, Some(last)) = (lane.is_main(), index.last) {
            locked(&self.history.positions).insert(last, index.ops);
        }
    }

    /// The checkpoints of a lane, in ID order.
    fn checkpoints_of(&self, lane: LaneId) -> Vec<Checkpoint> {
        self.store.lane_checkpoints(lane).cloned().collect()
    }

    /// Makes sure the index knows what the checkpoint `all[at]` changed:
    /// one the replay did not meet (its head is not an op of its lane) is
    /// compared with the one before it.
    fn known_changes(&self, index: &mut Index, all: &[Checkpoint], at: usize) -> Result<()> {
        let ck = &all[at];
        if index.changes.contains_key(&ck.id) {
            return Ok(());
        }
        let before = match (at.checked_sub(1).map(|at| &all[at]), index.start) {
            (Some(previous), _) => self.state_of(previous.head)?.0,
            (None, Some(start)) => self.state_of(start)?.0,
            (None, None) => Tree::new(),
        };
        let after = self.state_of(ck.head)?.0;
        index.changes.insert(ck.id, changes_between(&before, &after));
        Ok(())
    }

    /// Counts the lines of a change, once.
    fn lines_of(&self, changed: &mut Changed) -> Result<(usize, usize)> {
        if let Some(lines) = changed.lines {
            return Ok(lines);
        }
        let read = |entry: Option<TreeEntry>| -> Result<Vec<u8>> {
            match entry {
                Some(entry) => Ok(self.store.get_blob(&entry.blob)?.unwrap_or_default()),
                None => Ok(Vec::new()),
            }
        };
        let (old, new) = (read(changed.before)?, read(changed.after)?);
        let lines = match kito_diff::text::is_binary(&old) || kito_diff::text::is_binary(&new) {
            true => (0, 0),
            false => crate::review::line_counts(&old, &new),
        };
        changed.lines = Some(lines);
        Ok(lines)
    }

    /// The lane whose integration made a checkpoint: kept since Kito 0.41,
    /// before that read from the reason of its last op
    /// (`integrate lane/3: …`, recorded by the lane's agent).
    fn integrated_lane(&self, ck: &Checkpoint) -> Option<LaneId> {
        if ck.integrated.is_some() {
            return ck.integrated;
        }
        let op = self.store.get_op(&ck.head).ok()??;
        let lane = integrate_of(self.words(&op)?)?;
        let record = self.store.lane_record(lane)?;
        (record.agent.name == ck.created_by.name && op.author.name == record.agent.name).then_some(lane)
    }

    /// Why a checkpoint made by an integration was made: the intent of the
    /// lane's last change in that integration, which the first op of the
    /// integration has as its second parent.
    fn integration_why(&self, ck: &Checkpoint) -> Option<String> {
        let head = self.store.get_op(&ck.head).ok()??;
        integrate_of(self.words(&head)?)?;
        let intent = head.intent;
        let mut at = head;
        for _ in 0..BETWEEN_FOLLOWED {
            if let Some(merged) = at.parents.get(1) {
                let last = self.store.get_op(merged).ok()??;
                return self.words(&last).filter(|words| integrate_of(words).is_none()).map(str::to_owned);
            }
            let parent = self.store.get_op(at.parents.first()?).ok()??;
            if parent.intent != intent {
                return None;
            }
            at = parent;
        }
        None
    }

    /// A lane's record as it is now: an open lane's intent is kept with
    /// its working state.
    pub(crate) fn record_now(&self, lane: LaneId) -> Option<Lane> {
        match self.lanes.get(&lane) {
            Some(at) => {
                let mut record = at.record.clone();
                record.intent = at.work.intent;
                Some(record)
            }
            None => self.store.lane_record(lane).cloned(),
        }
    }

    /// One checkpoint as a history shows it; with `whole`, its files.
    fn checkpoint_info(
        &self,
        index: &mut Index,
        all: &[Checkpoint],
        at: usize,
        whole: bool,
    ) -> Result<CheckpointInfo> {
        self.known_changes(index, all, at)?;
        let ck = &all[at];
        let mut changed = index.changes.remove(&ck.id).unwrap_or_default();
        let mut files = Vec::with_capacity(changed.len());
        let (mut added, mut removed) = (0, 0);
        // What was worked out is put back, whatever the counting comes to.
        let counted = changed.iter_mut().try_for_each(|one| -> Result<()> {
            let (plus, minus) = self.lines_of(one)?;
            added += plus;
            removed += minus;
            if whole {
                files.push(ChangedFile {
                    path: shown(&one.path),
                    status: match (one.before, one.after, &one.from) {
                        (_, _, Some(_)) => "renamed",
                        (None, _, _) => "added",
                        (_, None, _) => "removed",
                        _ => "modified",
                    }
                    .to_owned(),
                    from: one.from.as_deref().map(shown),
                    lines_added: plus,
                    lines_removed: minus,
                    before: one.before.map(|entry| entry.blob.to_string()),
                    after: one.after.map(|entry| entry.blob.to_string()),
                });
            }
            Ok(())
        });
        let count = changed.len();
        index.changes.insert(ck.id, changed);
        counted?;
        let from = self.integrated_lane(ck);
        let lane = from.and_then(|lane| self.record_now(lane));
        let at_ms = ck.at_ms.or_else(|| self.store.get_op(&ck.head).ok().flatten().map(|op| op.at.wall_ms));
        Ok(CheckpointInfo {
            id: ck.id.to_string(),
            label: ck.label.clone(),
            at_ms,
            by: ck.created_by.name.clone(),
            on: (!ck.lane_of().is_main()).then(|| self.lane_name(ck.lane_of())),
            lane: from.map(|lane| self.lane_name(lane)),
            agent: lane.as_ref().map(|lane| lane.agent.name.clone()),
            person: lane.as_ref().and_then(|lane| lane.owner.as_ref()).map(|owner| owner.name.clone()),
            assistant: lane.as_ref().and_then(|lane| lane.assistant.clone()),
            task: lane.as_ref().and_then(|lane| lane.task.clone()),
            why: lane.is_some().then(|| self.integration_why(ck).unwrap_or_else(|| ck.label.clone())),
            files: count,
            lines_added: added,
            lines_removed: removed,
            approved_by: ck.approved_by.as_ref().map(|person| person.name.clone()),
            reviewed_by: ck.reviewed_by.iter().map(|person| person.name.clone()).collect(),
            checks: ck.verification.as_ref().map(|done| Checks {
                commands: done.commands.clone(),
                passed: done.passed,
                duration_ms: done.duration_ms,
            }),
            git_commit: ck.git_commit.as_ref().map(|oid| oid.iter().map(|b| format!("{b:02x}")).collect()),
            parent: at.checked_sub(1).map(|at| all[at].id.to_string()),
            changed: whole.then_some(files),
        })
    }

    /// A checkpoint given as `ckpt#4`.
    fn checkpoint_asked(&self, spec: &str) -> Result<Checkpoint> {
        let spec = spec.trim();
        let id: CheckpointId =
            spec.strip_prefix("ckpt#").and_then(|n| n.parse().ok()).map(CheckpointId).ok_or_else(|| {
                Error::Other(format!("{spec:?} is not a checkpoint: they are named ckpt#4"))
            })?;
        self.store
            .checkpoint(id)
            .cloned()
            .ok_or_else(|| Error::Other(format!("{id}: no such checkpoint (`kito history` lists them)")))
    }

    /// The checkpoints of the main lane, or of a permanent lane, newest
    /// first: `limit` of them, those older than `before`, those that
    /// changed `path` or something under it.
    pub fn checkpoints(
        &self,
        lane: LaneId,
        limit: usize,
        before: Option<&str>,
        path: Option<&str>,
    ) -> Result<CheckpointPage> {
        if !self.is_permanent(lane) {
            return Err(Error::Other(format!(
                "{} is not a permanent lane: only the main lane and permanent lanes have checkpoints",
                self.lane_name(lane)
            )));
        }
        let limit = limit.clamp(1, LISTED_AT_MOST);
        let before = match before {
            Some(spec) => {
                let ck = self.checkpoint_asked(spec)?;
                if ck.lane_of() != lane {
                    return Err(Error::Other(format!(
                        "{} is not a checkpoint of {}",
                        ck.id,
                        self.lane_name(lane)
                    )));
                }
                Some(ck.id)
            }
            None => None,
        };
        let path = path.map(path_asked).filter(|path| !path.as_os_str().is_empty());
        let all = self.checkpoints_of(lane);
        self.with_index(lane, |repo, index| {
            let mut found = Vec::new();
            let mut more = false;
            for at in (0..all.len()).rev() {
                if before.is_some_and(|before| all[at].id >= before) {
                    continue;
                }
                if let Some(path) = &path {
                    repo.known_changes(index, &all, at)?;
                    if !index.changes[&all[at].id].iter().any(|changed| changed.touches(path)) {
                        continue;
                    }
                }
                if found.len() == limit {
                    more = true;
                    break;
                }
                found.push(repo.checkpoint_info(index, &all, at, false)?);
            }
            Ok(CheckpointPage { checkpoints: found, more })
        })
    }

    /// One checkpoint, with the files it changed.
    pub fn checkpoint_whole(&self, spec: &str) -> Result<CheckpointInfo> {
        let ck = self.checkpoint_asked(spec)?;
        let all = self.checkpoints_of(ck.lane_of());
        let at = all.iter().position(|one| one.id == ck.id).ok_or(Error::NothingToCheckpoint)?;
        self.with_index(ck.lane_of(), |repo, index| repo.checkpoint_info(index, &all, at, true))
    }

    /// The state at a checkpoint, rebuilt from the nearest one kept: the
    /// lane's index says where on it each checkpoint is.
    fn checkpoint_state(&self, ck: &Checkpoint) -> Result<State> {
        self.with_index(ck.lane_of(), |_, _| Ok(()))?;
        self.state_of(ck.head)
    }

    /// A point given as `ckpt#4`, `main`, or a lane (`lane/3`, `staging`;
    /// a closed one too).
    pub fn point(&self, spec: &str) -> Result<Point> {
        let spec = spec.trim();
        if spec.starts_with("ckpt#") {
            return Ok(Point::Checkpoint(self.checkpoint_asked(spec)?.id));
        }
        match self.known_lane(spec)? {
            lane if lane.is_main() => Ok(Point::Main),
            lane => Ok(Point::Lane(lane)),
        }
    }

    /// How a point is named in answers.
    pub fn point_name(&self, point: Point) -> String {
        match point {
            Point::Checkpoint(id) => id.to_string(),
            Point::Main => "main".to_owned(),
            Point::Lane(lane) => self.lane_name(lane),
        }
    }

    /// The files and symbols at a point, and the op it is right after.
    fn point_state(&self, point: Point) -> Result<(State, Option<OpId>)> {
        match point {
            Point::Checkpoint(id) => {
                let ck = self.store.checkpoint(id).cloned().ok_or(Error::NothingToCheckpoint)?;
                Ok((self.checkpoint_state(&ck)?, Some(ck.head)))
            }
            Point::Main => Ok(((self.main.tree.clone(), self.main.symbols.clone()), self.main.head)),
            Point::Lane(lane) => match self.lanes.get(&lane) {
                Some(at) => Ok((
                    (at.work.tree.clone(), at.work.symbols.clone()),
                    Some(at.work.head.unwrap_or(at.record.base_head)),
                )),
                None => {
                    let record = self.store.lane_record(lane).ok_or(Error::NoSuchLane(lane))?;
                    Ok((self.state_of(record.head)?, Some(record.head)))
                }
            },
        }
    }

    /// The files of a point, as `lane_files` lists them.
    pub(crate) fn tree_of(&self, point: Point) -> Result<Tree> {
        Ok(self.point_state(point)?.0 .0)
    }

    /// The ops from `from` (excluded) to `to`, oldest first, with the ops
    /// of the lanes an integration among them brought in; none when
    /// `from` is not behind `to`. `from` absent: back to the first op.
    /// Two checkpoints of the main lane are read as the range between
    /// them; otherwise first parents are followed, at most
    /// [`BETWEEN_FOLLOWED`] ops. A diff without its reasons is still the
    /// diff.
    fn ops_between(&self, from: Option<OpId>, to: Option<OpId>) -> Result<Vec<Op>> {
        let Some(to) = to else { return Ok(Vec::new()) };
        let Some(last) = self.store.get_op(&to)? else { return Ok(Vec::new()) };
        if let Some(from) = from {
            // An op is never older than one it was made on top of.
            match self.store.get_op(&from)? {
                Some(first) if first.at <= last.at => {}
                _ => return Ok(Vec::new()),
            }
            let places = {
                let positions = locked(&self.history.positions);
                (positions.get(&from).copied(), positions.get(&to).copied())
            };
            if let (Some(start), Some(end)) = places {
                if start > end {
                    return Ok(Vec::new());
                }
                let mut found = Vec::new();
                for op in self.store.lane_ops(LaneId::MAIN, start, (end - start) as usize)? {
                    if let Some(merged) = op.parents.get(1).copied() {
                        let mut steps = 0;
                        let (mut brought, _) = self.brought_by(merged, None, &mut steps)?;
                        brought.reverse();
                        found.extend(brought);
                    }
                    found.push(op);
                }
                return Ok(found);
            }
        }
        let mut found: Vec<Op> = Vec::new();
        let (mut at, mut steps) = (to, 0usize);
        loop {
            if Some(at) == from {
                break;
            }
            if steps >= BETWEEN_FOLLOWED {
                return Ok(Vec::new());
            }
            let Some(op) = self.store.get_op(&at)? else { return Ok(Vec::new()) };
            steps += 1;
            let (first, merged) = (op.parents.first().copied(), op.parents.get(1).copied());
            found.push(op);
            if let Some(merged) = merged {
                let (brought, met) = self.brought_by(merged, from, &mut steps)?;
                found.extend(brought);
                if met {
                    break;
                }
            }
            match first {
                Some(parent) => at = parent,
                None if from.is_none() => break,
                None => return Ok(Vec::new()),
            }
        }
        found.reverse();
        Ok(found)
    }

    /// What an integration brought: the lane's own ops from its head
    /// `merged` back to where it last started from, newest first; and
    /// whether `from` was among them (the ops after it only, then).
    fn brought_by(&self, merged: OpId, from: Option<OpId>, steps: &mut usize) -> Result<(Vec<Op>, bool)> {
        let mut found = Vec::new();
        let lane = self.store.get_op(&merged)?.map(|op| op.lane);
        let mut at = merged;
        while *steps < BETWEEN_FOLLOWED {
            if Some(at) == from {
                return Ok((found, true));
            }
            let Some(op) = self.store.get_op(&at)?.filter(|op| Some(op.lane) == lane) else { break };
            *steps += 1;
            let parent = op.parents.first().copied();
            found.push(op);
            match parent {
                Some(parent) => at = parent,
                None => break,
            }
        }
        Ok((found, false))
    }

    /// What differs between two points: `to` against `from`, or, without
    /// `from`, a checkpoint against the one before it on its lane.
    pub fn diff(&self, to: &str, from: Option<&str>, paths: &[String]) -> Result<Diff> {
        let to = self.point(to)?;
        // The first checkpoint of a permanent lane is compared with what
        // the lane started from, as what it changed is.
        let mut started: Option<OpId> = None;
        let from = match (from, to) {
            (Some(spec), _) => Some(self.point(spec)?),
            (None, Point::Checkpoint(id)) => {
                let lane = self.store.checkpoint(id).map_or(LaneId::MAIN, Checkpoint::lane_of);
                let before = self.store.lane_checkpoints(lane).rfind(|ck| ck.id < id);
                if before.is_none() && !lane.is_main() {
                    started = self.with_index(lane, |_, index| Ok(index.start))?;
                }
                before.map(|ck| Point::Checkpoint(ck.id)).or_else(|| {
                    // Named by the checkpoint that is there, if one is.
                    let start = started?;
                    self.store
                        .all_checkpoints()
                        .find(|ck| ck.head == start)
                        .map(|ck| Point::Checkpoint(ck.id))
                })
            }
            (None, _) => {
                return Err(Error::Other(format!(
                    "a diff to {} needs `from`: a checkpoint, `main` or a lane",
                    self.point_name(to)
                )))
            }
        };
        if from == Some(to) {
            return Err(Error::Other(format!(
                "{} is compared with itself: give two different points",
                self.point_name(to)
            )));
        }
        let (after, to_head) = self.point_state(to)?;
        let (before, from_head) = match (from, started) {
            (Some(from), _) => self.point_state(from)?,
            (None, Some(start)) => (self.state_of(start)?, Some(start)),
            (None, None) => (State::default(), None),
        };
        let mut said = Said::default();
        for op in self.ops_between(from_head, to_head)? {
            said.take(self, &op);
        }
        let files = self.diff_files(
            Side { tree: &before.0, symbols: &before.1 },
            Side { tree: &after.0, symbols: &after.1 },
            said,
            paths,
        )?;
        Ok(Diff { from: from.map(|from| self.point_name(from)), to: self.point_name(to), files })
    }

    /// What a directory holds at a point (the main lane as it is, by
    /// default), with the last checkpoint that changed each entry.
    pub fn tree_listing(&self, at: Option<&str>, path: Option<&str>) -> Result<Listing> {
        let point = match at {
            Some(spec) => self.point(spec)?,
            None => Point::Main,
        };
        let tree = self.tree_of(point)?;
        let dir = path_asked(path.unwrap_or_default());
        let at_name = self.point_name(point);
        if !dir.as_os_str().is_empty() {
            if tree.contains(&dir) {
                return Err(Error::Other(format!(
                    "{} is a file at {at_name}, not a directory: `get_blobs` gives its content",
                    shown(&dir)
                )));
            }
            if tree.under(&dir).next().is_none() {
                return Err(Error::Other(format!("{}: no such directory at {at_name}", shown(&dir))));
            }
        }
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        let mut files: BTreeMap<String, TreeEntry> = BTreeMap::new();
        let inside: Box<dyn Iterator<Item = (&Path, &TreeEntry)>> = match dir.as_os_str().is_empty() {
            true => Box::new(tree.iter()),
            false => Box::new(tree.under(&dir)),
        };
        for (path, entry) in inside {
            let Ok(rest) = path.strip_prefix(&dir) else { continue };
            let mut parts = rest.components();
            let Some(Component::Normal(name)) = parts.next() else { continue };
            let name = name.to_string_lossy().into_owned();
            match parts.next() {
                Some(_) => {
                    dirs.insert(name);
                }
                None => {
                    files.insert(name, *entry);
                }
            }
        }
        let mut last = self.last_changes(point, &dir, dirs.iter().chain(files.keys()).cloned().collect())?;
        let readme = readme_of(files.keys().map(String::as_str));
        let mut entries: Vec<DirEntry> = dirs
            .into_iter()
            .map(|name| DirEntry {
                last: last.remove(&name),
                name,
                kind: "dir".to_owned(),
                blob: None,
                executable: false,
                symlink: false,
            })
            .collect();
        entries.extend(files.into_iter().map(|(name, entry)| DirEntry {
            last: last.remove(&name),
            name,
            kind: "file".to_owned(),
            blob: Some(entry.blob.to_string()),
            executable: entry.mode == FileMode::Executable,
            symlink: entry.mode == FileMode::Symlink,
        }));
        Ok(Listing { at: at_name, path: shown(&dir), entries, readme })
    }

    /// For each name of a directory: the last checkpoint at or before the
    /// point that changed it, or anything under it. One pass backwards,
    /// over at most [`LAST_LOOKED`] checkpoints, that stops when every
    /// name has one.
    fn last_changes(
        &self,
        point: Point,
        dir: &Path,
        names: BTreeSet<String>,
    ) -> Result<BTreeMap<String, Last>> {
        // Whose checkpoints, and up to which.
        let (lane, upto) = match point {
            Point::Checkpoint(id) => {
                (self.store.checkpoint(id).map_or(LaneId::MAIN, Checkpoint::lane_of), Upto::Checkpoint(id))
            }
            Point::Main => (LaneId::MAIN, Upto::All),
            Point::Lane(lane) if self.is_permanent(lane) => (lane, Upto::All),
            Point::Lane(lane) => match self.record_now(lane) {
                // On a permanent lane: its checkpoints made before the lane
                // was last brought up to it.
                Some(record)
                    if self.is_permanent(record.parent_lane()) && !record.parent_lane().is_main() =>
                {
                    let then = self.store.get_op(&record.base_head)?.map(|op| op.at);
                    (record.parent_lane(), then.map_or(Upto::All, Upto::Made))
                }
                // On the main lane: the checkpoint it is filed under.
                Some(record) => (LaneId::MAIN, Upto::Checkpoint(record.base)),
                None => (LaneId::MAIN, Upto::All),
            },
        };
        let all = self.checkpoints_of(lane);
        let newest = match upto {
            Upto::All => all.len().checked_sub(1),
            Upto::Checkpoint(upto) => all.iter().rposition(|ck| ck.id <= upto),
            Upto::Made(then) => all.iter().rposition(|ck| {
                self.store.get_op(&ck.head).ok().flatten().is_some_and(|head| head.at <= then)
            }),
        };
        self.with_index(lane, |repo, index| {
            let mut found: BTreeMap<String, Last> = BTreeMap::new();
            let Some(newest) = newest else { return Ok(found) };
            for at in (0..=newest).rev().take(LAST_LOOKED) {
                if found.len() == names.len() {
                    break;
                }
                repo.known_changes(index, &all, at)?;
                let mut hit: BTreeSet<String> = BTreeSet::new();
                for changed in &index.changes[&all[at].id] {
                    for path in std::iter::once(&changed.path).chain(changed.from.as_ref()) {
                        let Ok(rest) = path.strip_prefix(dir) else { continue };
                        if let Some(Component::Normal(name)) = rest.components().next() {
                            let name = name.to_string_lossy().into_owned();
                            if names.contains(&name) && !found.contains_key(&name) {
                                hit.insert(name);
                            }
                        }
                    }
                }
                if hit.is_empty() {
                    continue;
                }
                let ck = &all[at];
                let why = repo.integration_why(ck).unwrap_or_else(|| ck.label.clone());
                let at_ms =
                    ck.at_ms.or_else(|| repo.store.get_op(&ck.head).ok().flatten().map(|op| op.at.wall_ms));
                for name in hit {
                    found.insert(
                        name,
                        Last {
                            checkpoint: ck.id.to_string(),
                            why: why.clone(),
                            by: ck.created_by.name.clone(),
                            at_ms,
                        },
                    );
                }
            }
            Ok(found)
        })
    }

    // ------------------------------------------------------ closed lanes

    /// A lane given as `lane/1`, `1`, `main` or a permanent lane's name,
    /// open or closed. One that never was is no lane.
    pub fn known_lane(&self, spec: &str) -> Result<LaneId> {
        match self.lane_of(spec) {
            Err(Error::NoSuchLane(lane)) if self.store.lane_record(lane).is_some() => Ok(lane),
            other => other,
        }
    }

    /// Whether the lane is one that was closed.
    pub fn is_closed(&self, lane: LaneId) -> bool {
        !lane.is_main() && !self.lanes.contains_key(&lane) && self.store.lane_record(lane).is_some()
    }

    /// How many ops a closed lane recorded, and the files they touched.
    pub(crate) fn closed_summary(&self, lane: LaneId) -> Result<(u64, Vec<String>)> {
        if let Some(known) = locked(&self.history.closed).get(&lane) {
            return Ok(known.clone());
        }
        let ops = self.store.lane_len(lane)?;
        let mut files: BTreeSet<String> = BTreeSet::new();
        self.lane_ops_from(lane, 0, |op| files.extend(op.kind.paths().iter().map(|path| shown(path))))?;
        let summary = (ops, files.into_iter().collect::<Vec<_>>());
        locked(&self.history.closed).insert(lane, summary.clone());
        Ok(summary)
    }

    /// The integrations of a lane: those it keeps, or, for a lane from
    /// before Kito 0.41, those the history says.
    pub(crate) fn integrations_of(&self, record: &Lane) -> Vec<Integration> {
        match record.ends_kept {
            true => record.integrations.clone(),
            false => self.derived_for(record.id),
        }
    }

    /// Before the lane records an integration: the ones it had before
    /// Kito 0.41 are taken from the history first, and kept from now on.
    pub(crate) fn keep_ends(&mut self, lane: LaneId) {
        let Some(false) = self.lanes.get(&lane).map(|at| at.record.ends_kept) else { return };
        let earlier = self.derived_for(lane);
        if let Some(at) = self.lanes.get_mut(&lane) {
            at.record.integrations = earlier;
            at.record.ends_kept = true;
        }
    }

    /// Notes that a lane's work landed: written with the lane's record
    /// when it is brought up to date.
    pub(crate) fn note_integration(
        &mut self,
        lane: LaneId,
        into: LaneId,
        checkpoint: Option<CheckpointId>,
        by: Option<kito_core::Author>,
    ) -> Result<()> {
        self.keep_ends(lane);
        let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
        at.record.integrations.push(Integration { into, checkpoint, at_ms: crate::tasks::now_ms(), by });
        Ok(())
    }

    /// The integrations of a lane the history shows, read once for all: the ops a lane's
    /// agent recorded on another lane under `integrate lane/N: …`, the
    /// first with the lane's work as its second parent, and the checkpoint
    /// made on the last of them. Who asked is known only when the
    /// checkpoint says who let it in.
    fn derived_for(&self, lane: LaneId) -> Vec<Integration> {
        if locked(&self.history.derived).is_none() {
            let found = self.derived_integrations();
            *locked(&self.history.derived) = Some(found);
        }
        locked(&self.history.derived).as_ref().and_then(|found| found.get(&lane).cloned()).unwrap_or_default()
    }

    /// Reads the history for the integrations of [`Repo::derived_for`].
    fn derived_integrations(&self) -> BTreeMap<LaneId, Vec<Integration>> {
        let mut found: BTreeMap<LaneId, Vec<Integration>> = BTreeMap::new();
        // The integration going on on each lane: whose, under which
        // intent, and its last op so far.
        let mut going: HashMap<LaneId, (LaneId, Option<kito_core::IntentId>, usize, OpId)> = HashMap::new();
        let mut ends: Vec<(LaneId, usize, OpId)> = Vec::new();
        let agents: HashMap<LaneId, String> =
            self.store.lane_records().map(|record| (record.id, record.agent.name.clone())).collect();
        let scanned = self.store.scan_ops(|_, op| {
            let starts = self
                .words(&op)
                .and_then(integrate_of)
                .filter(|lane| *lane != op.lane && op.parents.len() > 1)
                .filter(|lane| agents.get(lane) == Some(&op.author.name));
            let under = going.get(&op.lane).map(|now| now.1);
            match (starts, under) {
                (Some(lane), _) => {
                    if let Some((was, _, at, last)) = going.remove(&op.lane) {
                        ends.push((was, at, last));
                    }
                    let list = found.entry(lane).or_default();
                    list.push(Integration {
                        into: op.lane,
                        checkpoint: None,
                        at_ms: op.at.wall_ms,
                        by: None,
                    });
                    going.insert(op.lane, (lane, op.intent, list.len() - 1, op.id));
                }
                (None, Some(intent)) if intent == op.intent => {
                    if let Some(now) = going.get_mut(&op.lane) {
                        now.3 = op.id;
                    }
                }
                (None, Some(_)) => {
                    if let Some((was, _, at, last)) = going.remove(&op.lane) {
                        ends.push((was, at, last));
                    }
                }
                (None, None) => {}
            }
            Ok(())
        });
        if scanned.is_err() {
            return BTreeMap::new();
        }
        ends.extend(going.into_values().map(|(lane, _, at, last)| (lane, at, last)));
        for (lane, at, last) in ends {
            let Some(integration) = found.get_mut(&lane).and_then(|list| list.get_mut(at)) else { continue };
            let made =
                self.store.all_checkpoints().find(|ck| ck.head == last && ck.lane_of() == integration.into);
            if let Some(ck) = made {
                integration.checkpoint = Some(ck.id);
                integration.by = ck.approved_by.clone();
            }
        }
        found
    }

    /// Runs `f` with a closed lane standing as it was when it closed, as
    /// if it were open: for the questions that read an open lane. Nothing
    /// is written meanwhile, and the lane is gone again after.
    pub(crate) fn with_closed_lane<T>(
        &mut self,
        lane: LaneId,
        f: impl FnOnce(&mut Repo) -> Result<T>,
    ) -> Result<T> {
        let record = self.store.lane_record(lane).cloned().ok_or(Error::NoSuchLane(lane))?;
        // A stand-in for an open lane would replace it, then remove it; one
        // made of a record not closed would be scanned, its working copy gone.
        if self.lanes.contains_key(&lane) || record.state != kito_core::LaneState::Closed {
            return Err(Error::Other(format!("{lane} is not a closed lane: it is asked about as it is")));
        }
        let (base_tree, base_symbols) = self.state_of(record.base_head)?;
        let (tree, symbols) = self.state_of(record.head)?;
        let mut work =
            crate::repo::Work::new(lane, self.lane_dir(lane).join("work"), record.agent.clone(), &[]);
        (work.tree, work.symbols, work.head) = (tree, symbols, Some(record.head));
        work.ops = self.store.lane_len(lane)?;
        work.intent = record.intent;
        self.lanes.insert(lane, crate::lanes::LaneAt { record, work, base_tree, base_symbols });
        // Gone again however `f` ends, a panic included: a closed lane
        // left among the open ones could be written to.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        self.lanes.remove(&lane);
        self.claims_cache.lock().unwrap_or_else(|e| e.into_inner()).remove(&lane);
        self.told.remove(&lane);
        self.reasons_told.remove(&lane);
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}

impl Index {
    /// The checkpoints `ids` are made on the last op replayed: the first
    /// changed what was touched since the one before, the others nothing.
    fn checkpoint_here(&mut self, ids: &[CheckpointId]) {
        let changed = self.close();
        for (n, id) in ids.iter().enumerate() {
            self.changes.insert(*id, if n == 0 { changed.clone() } else { Vec::new() });
        }
    }
}

/// Up to where checkpoints are looked through.
#[derive(Clone, Copy)]
enum Upto {
    All,
    Checkpoint(CheckpointId),
    /// Those made by then.
    Made(kito_core::Hlc),
}

/// The lane an integration's reason names: `integrate lane/3: …` or
/// `integrate lane/3 (agent)`.
pub(crate) fn integrate_of(summary: &str) -> Option<LaneId> {
    let rest = summary.strip_prefix("integrate lane/")?;
    let digits: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
    match rest[digits.len()..].chars().next() {
        Some(':') | Some(' ') => u64::from_str_radix(&digits, 16).ok().map(LaneId),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kito_core::paths::Canonical as _;

    #[test]
    fn a_closed_lane_asked_about_is_closed_again_whatever_the_question_did() {
        std::env::set_var("KITO_AUTHOR", "ada");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonical().unwrap();
        drop(Repo::init(&root).unwrap());
        let mut repo = Repo::open(&root).unwrap();
        std::fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
        std::fs::write(root.join("shop.py"), "def total(prices):\n    return sum(prices)\n").unwrap();
        repo.checkpoint("start").unwrap();
        let opened = repo.open_lane("alice", None, None, "totals in cents", &["./".to_owned()]).unwrap();
        let lane: LaneId = opened.id.parse().unwrap();
        repo.close_lane(lane).unwrap();

        // A question that panics half-way.
        let asked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = repo.with_closed_lane(lane, |repo| -> Result<()> {
                assert!(repo.lanes.contains_key(&lane));
                panic!("a question that went wrong");
            });
        }));
        assert!(asked.is_err());
        // The lane is closed still: not open, not to be written to; and it answers.
        assert!(!repo.lanes.contains_key(&lane) && repo.is_closed(lane));
        assert!(matches!(repo.lane_of("lane/1"), Err(Error::NoSuchLane(_))));
        assert!(repo.set_intent(lane, "something else entirely", None, None).is_err());
        assert!(repo.why_graph(lane, "shop.py:total", false).is_ok());
        assert!(!repo.lanes.contains_key(&lane));

        // Only a closed lane stands in: an open one is refused, and stays open.
        let open = repo.open_lane("bob", None, None, "totals in whole cents", &["./".to_owned()]).unwrap();
        let open: LaneId = open.id.parse().unwrap();
        let refused = repo.with_closed_lane(open, |_| Ok(())).unwrap_err();
        assert!(refused.to_string().contains("not a closed lane"), "{refused}");
        assert!(repo.lanes.contains_key(&open) && repo.lane_of("lane/2").is_ok());
    }

    #[test]
    fn an_integration_is_read_from_its_reason() {
        assert_eq!(integrate_of("integrate lane/1a: prices in cents"), Some(LaneId(26)));
        assert_eq!(integrate_of("integrate lane/3 (alice)"), Some(LaneId(3)));
        assert_eq!(integrate_of("integrate lane/3"), None);
        assert_eq!(integrate_of("integrate the cache"), None);
    }

    #[test]
    fn the_readme_of_a_directory_is_found_whatever_its_case() {
        let names = ["a.rs", "readme.txt", "README", "README.md"];
        assert_eq!(readme_of(names.into_iter()), Some("README.md".to_owned()));
        assert_eq!(readme_of(["Readme.rst", "main.rs"].into_iter()), Some("Readme.rst".to_owned()));
        assert_eq!(readme_of(["readmes.md"].into_iter()), None);
        assert_eq!(path_asked("./src/"), PathBuf::from("src"));
        assert_eq!(path_asked("."), PathBuf::new());
    }
}
