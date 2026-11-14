//! Lanes: the working copies of the agents.
//!
//! A lane is a line of work that starts from a point of the main lane. It
//! has a working directory of its own (`.kito/lanes/<id>/work`), captured
//! like the main one, and its ops are recorded in the same log under its
//! own ID. What a lane holds is the state of the main lane at its starting
//! point plus its own ops since then: both can always be rebuilt from the
//! log, and so can the working directory.

use crate::error::{Error, Result};
use crate::repo::{FileStat, Repo, Work};
use kito_core::{
    codec, BlobId, Checkpoint, CheckpointId, Intent, Lane, LaneId, LaneState, OpId, SymbolTable, Tree,
    TreeEntry,
};
use kito_store::fs::atomic_write;
use kito_store::STORE_DIR;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Layout version of the saved state of a lane.
const STATE_VERSION: u32 = 1;

/// An open lane.
pub(crate) struct LaneAt {
    /// What the store last recorded about it. `head` is kept current in
    /// `work`, not here.
    pub(crate) record: Lane,
    pub(crate) work: Work,
    /// The state of the main lane the lane starts from (at `record.base_head`).
    pub(crate) base_tree: Tree,
    pub(crate) base_symbols: SymbolTable,
}

/// State of a lane saved for a quick restart: it can always be rebuilt from
/// the log.
#[derive(Serialize, Deserialize)]
struct SavedLane {
    version: u32,
    base_head: OpId,
    since: u64,
    base_tree: Tree,
    base_symbols: SymbolTable,
    /// Ops of the lane already applied to `tree` and `symbols`.
    ops: u64,
    head: Option<OpId>,
    tree: Tree,
    symbols: SymbolTable,
    last_clean: Vec<(PathBuf, BlobId)>,
    stat: Vec<(PathBuf, FileStat)>,
}

/// A lane as the clients see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneInfo {
    /// `lane/1`.
    pub id: String,
    pub agent: String,
    pub task: Option<String>,
    /// The "why" the next ops are recorded with.
    pub intent: Option<String>,
    /// The checkpoint of the main lane the lane is based on.
    pub base: String,
    /// `active`, `integrating`, `blocked` or `closed`.
    pub state: String,
    /// Where the agent works. `None` for a closed lane.
    pub workdir: Option<PathBuf>,
    /// Ops recorded since the base.
    pub ops: u64,
    /// Files that differ from the base.
    pub files: Vec<String>,
    /// Conflicts waiting for a decision.
    pub conflicts: Vec<String>,
}

fn state_word(state: LaneState) -> &'static str {
    match state {
        LaneState::Active => "active",
        LaneState::Ready => "ready",
        LaneState::Integrating => "integrating",
        LaneState::Blocked(_) => "blocked",
        LaneState::Closed => "closed",
    }
}

impl Repo {
    /// Ignore patterns of a lane's working copy: the configured ones, plus
    /// the shared paths (links to what the main directory has).
    pub(crate) fn lane_ignore(&self) -> Vec<String> {
        let shared = self.config.lanes.share.iter().map(|path| format!("/{}", path.trim_matches('/')));
        self.config.capture.ignore.iter().cloned().chain(shared).collect()
    }

    fn lane_dir(&self, lane: LaneId) -> PathBuf {
        self.root.join(STORE_DIR).join("lanes").join(format!("{:x}", lane.0))
    }

    /// A lane given as `lane/1`, `1` or `main`.
    pub fn lane_of(&self, spec: &str) -> Result<LaneId> {
        let lane: LaneId = spec
            .trim()
            .parse()
            .map_err(|_| Error::Other(format!("{spec:?} is not a lane (lane/1, main)")))?;
        if lane.is_main() || self.lanes.contains_key(&lane) {
            Ok(lane)
        } else {
            Err(Error::NoSuchLane(lane))
        }
    }

    /// The state of the main lane right after the op `head`.
    pub(crate) fn state_at(&self, head: OpId) -> Result<(Tree, SymbolTable)> {
        if self.main.head == Some(head) {
            return Ok((self.main.tree.clone(), self.main.symbols.clone()));
        }
        let (mut tree, mut symbols) = (Tree::new(), SymbolTable::new());
        let mut at = 0u64;
        loop {
            let page = self.store.lane_ops(LaneId::MAIN, at, 1000)?;
            if page.is_empty() {
                return Err(Error::Other(format!("the op {head} is not on the main lane")));
            }
            for op in page {
                tree.apply(&op.kind)?;
                symbols.apply(&op.kind)?;
                at += 1;
                if op.id == head {
                    return Ok((tree, symbols));
                }
            }
        }
    }

    /// Creates the working copy of a lane from a tree: files the main
    /// directory holds unchanged are cloned from there, the rest comes from
    /// the store. Returns what the file system says of the files created.
    pub(crate) fn materialize_tree(&self, tree: &Tree, workdir: &Path) -> Result<Vec<(PathBuf, FileStat)>> {
        kito_lane::remove(workdir).map_err(Error::io(workdir))?;
        // The main directory vouches for the files that have not changed
        // since their last capture.
        let holds = |path: &Path, entry: &TreeEntry| {
            self.main.tree.get(path) == Some(entry) && self.main.stat.contains_key(path)
        };
        let source = kito_lane::Source { root: &self.root, holds: &holds };
        let store = &self.store;
        let blob = |id: &BlobId| match store.get_blob(id) {
            Ok(Some(content)) => Ok(content),
            Ok(None) => Err(std::io::Error::other(format!("content {id} is not in the store"))),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        };
        let done = kito_lane::materialize(tree, workdir, Some(&source), &blob).map_err(Error::io(workdir))?;
        // What the tools need and is not tracked: seen through a link.
        for shared in &self.config.lanes.share {
            let shared = shared.trim_matches('/');
            let (from, to) = (self.root.join(shared), workdir.join(shared));
            if !shared.is_empty() && fs::symlink_metadata(&from).is_ok() && fs::symlink_metadata(&to).is_err()
            {
                if let Some(dir) = to.parent() {
                    fs::create_dir_all(dir).map_err(Error::io(dir))?;
                }
                std::os::unix::fs::symlink(&from, &to).map_err(Error::io(&to))?;
            }
        }
        Ok(done.stats.into_iter().map(|(path, meta)| (path, FileStat::of(&meta))).collect())
    }

    /// Opens a lane for an agent. It starts from the current state of the
    /// main lane, or from the checkpoint given as `base`.
    pub fn open_lane(&mut self, agent: &str, task: Option<String>, base: Option<&str>) -> Result<LaneInfo> {
        let agent = agent.trim();
        if agent.is_empty() {
            return Err(Error::Other("a lane needs the name of the agent that works in it".into()));
        }
        self.scan()?;
        let main_head = self.main.head.ok_or_else(|| {
            Error::Other("the main lane is empty: there is nothing a lane could start from".into())
        })?;
        // The starting point, and the checkpoint it is filed under.
        let latest = self.store.checkpoints().next_back().map(|checkpoint| checkpoint.id);
        let (base, base_head) = match base {
            Some(spec) => {
                let id: CheckpointId =
                    spec.trim().parse().map_err(|_| Error::Other(format!("{spec:?} is not a checkpoint")))?;
                let checkpoint = self
                    .store
                    .checkpoint(id)
                    .ok_or_else(|| Error::Other(format!("{id}: this checkpoint does not exist")))?;
                (id, checkpoint.head)
            }
            None => match latest {
                Some(latest) => (latest, main_head),
                None => {
                    // A lane is filed under a checkpoint: the first one is made now.
                    let checkpoint = Checkpoint {
                        id: self.store.next_checkpoint_id(),
                        head: main_head,
                        label: "start".to_owned(),
                        created_by: self.main.author.clone(),
                        verification: None,
                        git_commit: None,
                    };
                    self.store.put_checkpoint(&checkpoint)?;
                    if self.config.git.export_on_checkpoint {
                        // The lane does not depend on the export: a failure is not its problem.
                        let _ = self.export_git();
                    }
                    (checkpoint.id, main_head)
                }
            },
        };
        let (base_tree, base_symbols) = self.state_at(base_head)?;

        let id = self.store.next_lane_id();
        let workdir = self.lane_dir(id).join("work");
        let stats = self.materialize_tree(&base_tree, &workdir)?;
        let author = kito_core::Author::agent(agent);
        let mut work = Work::new(id, workdir, author.clone(), &self.lane_ignore());
        work.tree = base_tree.clone();
        work.symbols = base_symbols.clone();
        work.head = Some(base_head);
        work.stat = stats.into_iter().collect();
        let record = Lane {
            id,
            agent: author,
            base,
            head: base_head,
            workdir: PathBuf::from(STORE_DIR).join("lanes").join(format!("{:x}", id.0)).join("work"),
            state: LaneState::Active,
            base_head,
            since: 0,
            task: task.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()),
            intent: None,
            next_symbol: 0,
        };
        self.store.put_lane(&record)?;
        self.lanes.insert(id, LaneAt { record, work, base_tree, base_symbols });
        self.save_lane(id)?;
        self.lane_info(id)
    }

    /// Records the last changes of a lane and closes it: its working copy
    /// goes away, its ops stay in the log.
    pub fn close_lane(&mut self, lane: LaneId) -> Result<LaneInfo> {
        if lane.is_main() {
            return Err(Error::Other("the main lane cannot be closed".into()));
        }
        self.scan_lane(lane)?;
        self.put_lane_state(lane, LaneState::Closed)?;
        let info = self.lane_info(lane)?;
        self.lanes.remove(&lane);
        let dir = self.lane_dir(lane);
        kito_lane::remove(&dir).map_err(Error::io(&dir))?;
        Ok(LaneInfo { workdir: None, ..info })
    }

    /// Records the lane as it is now, in the given state.
    pub(crate) fn put_lane_state(&mut self, lane: LaneId, state: LaneState) -> Result<()> {
        let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
        at.record.state = state;
        at.record.head = at.work.head.unwrap_or(at.record.base_head);
        at.record.intent = at.work.intent;
        at.record.next_symbol = at.record.next_symbol.max(at.work.symbols.next_seq(lane));
        let record = at.record.clone();
        self.store.put_lane(&record)?;
        Ok(())
    }

    /// Declares the "why" of the ops a lane records from now on.
    pub fn set_intent(
        &mut self,
        lane: LaneId,
        summary: &str,
        task: Option<String>,
        prompt: Option<&str>,
    ) -> Result<String> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err(Error::Other("an intent needs a summary".into()));
        }
        // What was saved under the previous intent is recorded under it.
        self.scan_lane(lane)?;
        let prompt = match prompt {
            Some(text) => Some(self.store.put_blob(text.as_bytes())?),
            None => None,
        };
        let intent = Intent::new(summary, task.filter(|t| !t.trim().is_empty()), prompt)?;
        self.store.put_intent(&intent)?;
        if lane.is_main() {
            self.main.intent = Some(intent.id);
        } else {
            let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
            at.work.intent = Some(intent.id);
            let state = at.record.state;
            self.put_lane_state(lane, state)?;
        }
        Ok(intent.id.to_string())
    }

    /// The open lanes; with `all`, the closed ones too.
    pub fn lanes(&self, all: bool) -> Vec<LaneInfo> {
        let mut list: Vec<LaneInfo> = self.lanes.keys().filter_map(|id| self.lane_info(*id).ok()).collect();
        if all {
            let closed = self.store.lane_records().filter(|record| !self.lanes.contains_key(&record.id));
            list.extend(closed.map(|record| self.describe_lane(record, 0, Vec::new(), None)));
            list.sort_by_key(|info| info.id.parse::<LaneId>().map_or(u64::MAX, |id| id.0));
        }
        list
    }

    fn describe_lane(
        &self,
        record: &Lane,
        ops: u64,
        files: Vec<String>,
        workdir: Option<PathBuf>,
    ) -> LaneInfo {
        LaneInfo {
            id: record.id.to_string(),
            agent: record.agent.name.clone(),
            task: record.task.clone(),
            intent: record.intent.and_then(|id| self.store.intent(&id)).map(|intent| intent.summary.clone()),
            base: record.base.to_string(),
            state: state_word(record.state).to_owned(),
            workdir,
            ops,
            files,
            conflicts: self
                .store
                .conflicts()
                .filter(|c| c.assigned_to == Some(record.id) && c.resolved_by.is_none())
                .map(|c| c.id.to_string())
                .collect(),
        }
    }

    pub fn lane_info(&self, lane: LaneId) -> Result<LaneInfo> {
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let mut record = at.record.clone();
        record.intent = at.work.intent;
        let mut files: BTreeSet<&Path> = BTreeSet::new();
        for (path, entry) in at.work.tree.iter() {
            if at.base_tree.get(path) != Some(entry) {
                files.insert(path);
            }
        }
        for (path, _) in at.base_tree.iter() {
            if !at.work.tree.contains(path) {
                files.insert(path);
            }
        }
        let files = files.into_iter().map(|p| p.display().to_string()).collect();
        Ok(self.describe_lane(&record, at.work.ops - record.since, files, Some(at.work.root.clone())))
    }

    /// Saves the state of a lane for the next open.
    pub(crate) fn save_lane(&self, lane: LaneId) -> Result<()> {
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let saved = SavedLane {
            version: STATE_VERSION,
            base_head: at.record.base_head,
            since: at.record.since,
            base_tree: at.base_tree.clone(),
            base_symbols: at.base_symbols.clone(),
            ops: at.work.ops,
            head: at.work.head,
            tree: at.work.tree.clone(),
            symbols: at.work.symbols.clone(),
            last_clean: at.work.last_clean.iter().map(|(p, b)| (p.clone(), *b)).collect(),
            // Like the main lane's: only what was not written an instant ago.
            stat: at.work.stat.iter().map(|(p, s)| (p.clone(), *s)).collect(),
        };
        let path = self.lane_dir(lane).join("state");
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(Error::io(dir))?;
        }
        atomic_write(&path, &codec::to_vec(&saved)?).map_err(Error::io(&path))
    }

    pub(crate) fn save_lanes(&self) -> Result<()> {
        self.lanes.keys().try_for_each(|lane| self.save_lane(*lane))
    }

    /// Rebuilds the state of every open lane: from its saved state if that
    /// still matches the log, from the log alone otherwise.
    pub(crate) fn load_lanes(&mut self) -> Result<()> {
        let records: Vec<Lane> =
            self.store.lane_records().filter(|record| record.state != LaneState::Closed).cloned().collect();
        let ignore = self.lane_ignore();
        for record in records {
            let lane = record.id;
            let total = self.store.lane_len(lane)?;
            let workdir = self.root.join(&record.workdir);
            let mut work = Work::new(lane, workdir.clone(), record.agent.clone(), &ignore);
            work.intent = record.intent;

            let path = self.lane_dir(lane).join("state");
            let saved = fs::read(&path).ok().and_then(|bytes| codec::from_slice::<SavedLane>(&bytes).ok());
            let usable = match &saved {
                Some(saved) => {
                    saved.version == STATE_VERSION
                        && saved.base_head == record.base_head
                        && saved.since == record.since
                        && (record.since..=total).contains(&saved.ops)
                        && match saved.ops == record.since {
                            true => saved.head == Some(record.base_head),
                            false => {
                                self.store.lane_ops(lane, saved.ops - 1, 1)?.first().map(|op| op.id)
                                    == saved.head
                            }
                        }
                }
                None => false,
            };
            let (base_tree, base_symbols) = match saved {
                Some(saved) if usable => {
                    work.tree = saved.tree;
                    work.symbols = saved.symbols;
                    work.last_clean = saved.last_clean.into_iter().collect();
                    work.stat = saved.stat.into_iter().collect();
                    work.head = saved.head;
                    work.ops = saved.ops;
                    (saved.base_tree, saved.base_symbols)
                }
                _ => {
                    let (tree, symbols) = self.state_at(record.base_head)?;
                    work.tree = tree.clone();
                    work.symbols = symbols.clone();
                    work.head = Some(record.base_head);
                    work.ops = record.since;
                    (tree, symbols)
                }
            };
            while work.ops < total {
                let page = self.store.lane_ops(lane, work.ops, 1000)?;
                if page.is_empty() {
                    break;
                }
                for op in page {
                    for path in op.kind.paths() {
                        work.stat.remove(path);
                    }
                    work.tree.apply(&op.kind)?;
                    work.symbols.apply(&op.kind)?;
                    work.head = Some(op.id);
                    work.ops += 1;
                }
            }
            // IDs the lane used before its last sync are not in this table.
            work.symbols.reserve(lane, record.next_symbol);
            // A working copy that is gone is made again, not taken for a
            // lane that removed every file.
            if !workdir.is_dir() {
                work.stat = self.materialize_tree(&work.tree, &workdir)?.into_iter().collect();
            }
            self.lanes.insert(lane, LaneAt { record, work, base_tree, base_symbols });
        }
        Ok(())
    }
}
