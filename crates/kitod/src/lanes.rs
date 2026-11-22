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
    /// What the lane declared it would change: functions
    /// (`src/auth.ts:getUser`), files, directories (`src/billing/`).
    #[serde(default)]
    pub scope: Vec<String>,
    /// What it changed outside that scope: it is not integrated until the
    /// scope is extended or the change undone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_scope: Vec<String>,
    /// The person the lane was opened for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Who asked for the lane to be integrated, when it waits for somebody
    /// who may do it (its `state` is then `awaiting_approval`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_by: Option<String>,
    /// The lane it started from, when that is not the main lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The lane it lands on, when that is not the main lane or when it was
    /// said: `lane/2`, `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The name of a permanent lane (`staging`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Where its review stands, when it has one or needs one (ADR 0004):
    /// "0 of 1 approval", "changes requested by bob", "approved by bob".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    /// The assistant its agent runs in, and the model, as last said
    /// (ADR 0010): `{"name": "claude-code", "version": "2.1.0", "model":
    /// "claude-opus-5"}`. What that side said of itself: a label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<kito_core::Assistant>,
}

/// Where a lane starts and where it lands, when not on the main lane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stack {
    /// The open lane it starts on. None: the main lane.
    pub on: Option<LaneId>,
    /// The lane it lands on. None: the one it starts on.
    pub target: Option<LaneId>,
}

/// Words that, alone, say that something changed but not why: an intent
/// made only of them is refused (D-145).
const SAYS_NOTHING: &[&str] = &[
    "a",
    "again",
    "all",
    "an",
    "and",
    "bug",
    "bugs",
    "change",
    "changed",
    "changes",
    "cleanup",
    "code",
    "commit",
    "done",
    "edit",
    "edited",
    "edits",
    "file",
    "files",
    "fix",
    "fixed",
    "fixes",
    "for",
    "improve",
    "improved",
    "improvements",
    "in",
    "it",
    "minor",
    "misc",
    "more",
    "of",
    "on",
    "progress",
    "quick",
    "refactor",
    "refactoring",
    "small",
    "some",
    "stuff",
    "temp",
    "test",
    "tests",
    "that",
    "the",
    "things",
    "this",
    "tmp",
    "to",
    "todo",
    "tweak",
    "tweaks",
    "update",
    "updated",
    "updates",
    "various",
    "wip",
    "work",
    "working",
    "x",
    "xx",
    "xxx",
];

/// Why an intent is refused, if it is: it says nothing of what the change
/// is for. `None`: it says something.
pub fn says_nothing(summary: &str) -> Option<String> {
    let words: Vec<String> = summary
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    let letters: usize = words.iter().map(|word| word.chars().filter(|c| c.is_alphabetic()).count()).sum();
    if letters >= 3 && words.iter().any(|word| !SAYS_NOTHING.contains(&word.as_str())) {
        return None;
    }
    Some(format!(
        "{:?} says that something changes, not why: the intent is what the next agent is told before it \
         changes this code. Say what the change is for (\"prices in cents everywhere\", \"fix the rounding \
         of totals\")",
        summary.trim()
    ))
}

/// `main` or `lane/2`, for messages.
pub(crate) fn lane_word(lane: LaneId) -> String {
    match lane.is_main() {
        true => "main".to_owned(),
        false => lane.to_string(),
    }
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
        self.config.ignore_patterns().into_iter().chain(shared).collect()
    }

    pub(crate) fn lane_dir(&self, lane: LaneId) -> PathBuf {
        self.root.join(STORE_DIR).join("lanes").join(format!("{:x}", lane.0))
    }

    /// A lane given as `lane/1`, `1` or `main`.
    pub fn lane_of(&self, spec: &str) -> Result<LaneId> {
        // A permanent lane by its name: `staging`.
        if let Some(lane) = self.permanent_lane(spec.trim()) {
            return Ok(lane);
        }
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

    /// The state right after the op `head`, of whatever lane: the first
    /// parents of an op lead back through its lane to the op the lane
    /// started from, and from there through the lanes below it to the main
    /// lane (ADR 0001). Those of the main lane are its log in order.
    pub(crate) fn state_at(&self, head: OpId) -> Result<(Tree, SymbolTable)> {
        let mut chain = Vec::new();
        let mut at = head;
        let (mut tree, mut symbols) = loop {
            if self.main.head == Some(at) {
                break (self.main.tree.clone(), self.main.symbols.clone());
            }
            // An open lane that is there already.
            if let Some(lane) = self.lanes.values().find(|lane| lane.work.head == Some(at)) {
                break (lane.work.tree.clone(), lane.work.symbols.clone());
            }
            if let Some(lane) = self.lanes.values().find(|lane| lane.record.base_head == at) {
                break (lane.base_tree.clone(), lane.base_symbols.clone());
            }
            let op = self
                .store
                .get_op(&at)?
                .ok_or_else(|| Error::Other(format!("the op {at} is not in the log")))?;
            if op.lane.is_main() {
                break self.main_state_at(at)?;
            }
            let first = op.parents.first().copied();
            chain.push(op);
            match first {
                Some(parent) => at = parent,
                None => break (Tree::new(), SymbolTable::new()),
            }
        };
        for op in chain.iter().rev() {
            tree.apply(&op.kind)?;
            symbols.apply(&op.kind)?;
        }
        Ok((tree, symbols))
    }

    /// The state of the main lane right after its op `head`.
    fn main_state_at(&self, head: OpId) -> Result<(Tree, SymbolTable)> {
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
        let holds = |path: &Path, entry: &TreeEntry, meta: &fs::Metadata| {
            self.main.tree.get(path) == Some(entry) && self.main.stat.get(path) == Some(&FileStat::of(meta))
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
                #[cfg(unix)]
                std::os::unix::fs::symlink(&from, &to).map_err(Error::io(&to))?;
                // Links need privileges on Windows; a directory junction
                // and a hard link do not (ADR 0002).
                #[cfg(windows)]
                match from.is_dir() {
                    true => junction::create(&from, &to).map_err(Error::io(&to))?,
                    false => fs::hard_link(&from, &to).map_err(Error::io(&to))?,
                }
            }
        }
        Ok(done.stats.into_iter().map(|(path, meta)| (path, FileStat::of(&meta))).collect())
    }

    /// Opens a lane for an agent. It starts from the current state of the
    /// main lane, or from the checkpoint given as `base`. No lane opens
    /// without saying why (`intent`) and what it will change (`scope`).
    pub fn open_lane(
        &mut self,
        agent: &str,
        task: Option<String>,
        base: Option<&str>,
        intent: &str,
        scope: &[String],
    ) -> Result<LaneInfo> {
        self.open_lane_on(agent, task, base, intent, scope, Stack::default())
    }

    /// The lanes from `lane` down to the main lane: its parent, the parent
    /// of that, …, the main lane last. `lane` itself is not in it.
    pub fn ancestors(&self, lane: LaneId) -> Vec<LaneId> {
        let mut found = Vec::new();
        let mut at = lane;
        while !at.is_main() {
            let parent = match self.lanes.get(&at) {
                Some(open) => open.record.parent_lane(),
                None => self.store.lane_record(at).map_or(LaneId::MAIN, |record| record.parent_lane()),
            };
            // A lane is never its own ancestor; a broken record does not loop.
            if found.contains(&parent) || parent == lane {
                break;
            }
            found.push(parent);
            at = parent;
        }
        if found.last() != Some(&LaneId::MAIN) {
            found.push(LaneId::MAIN);
        }
        found
    }

    /// The open lanes stacked directly on `lane`.
    pub fn children(&self, lane: LaneId) -> Vec<LaneId> {
        self.lanes
            .values()
            .filter(|open| open.record.parent_lane() == lane)
            .map(|open| open.record.id)
            .collect()
    }

    /// Like [`Repo::open_lane`], on top of another lane or landing on one
    /// (ADR 0001).
    pub fn open_lane_on(
        &mut self,
        agent: &str,
        task: Option<String>,
        base: Option<&str>,
        intent: &str,
        scope: &[String],
        stack: Stack,
    ) -> Result<LaneInfo> {
        let parent = stack.on.filter(|lane| !lane.is_main());
        if let Some(parent) = parent {
            if !self.lanes.contains_key(&parent) {
                return Err(Error::Other(format!(
                    "{parent} is not an open lane: a lane starts on an open one"
                )));
            }
            if base.is_some() {
                return Err(Error::Other(format!(
                    "a lane stacked on {parent} starts from it as it is: no checkpoint can be given"
                )));
            }
        }
        let target = match stack.target {
            None => None,
            Some(target) if target == parent.unwrap_or(LaneId::MAIN) => None,
            Some(target) => {
                let below = match parent {
                    Some(parent) => self.ancestors(parent),
                    None => vec![LaneId::MAIN],
                };
                if !below.contains(&target) {
                    return Err(Error::Other(format!(
                        "{target} is not below the lane: a lane lands on its parent or on a lane its parent \
                         started from"
                    )));
                }
                Some(target)
            }
        };
        self.open_lane_with(agent, task, base, intent, scope, parent, target, None)
    }

    /// Creates a permanent lane: `staging` next to `main`, starting from
    /// `from` (the main lane, or another permanent lane) as it is now. It
    /// has no scope, cannot be closed, and has checkpoints of its own
    /// (ADR 0001).
    pub fn create_lane(&mut self, name: &str, from: Option<LaneId>) -> Result<LaneInfo> {
        let name = name.trim();
        let valid = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
            && name.chars().any(|c| c.is_ascii_lowercase())
            && !name.starts_with(['-', '.']);
        if !valid || name == "main" || name.starts_with("lane") {
            return Err(Error::Other(format!(
                "{name:?} cannot name a permanent lane: lowercase letters, digits, `-`, `_` and `.`, not `main` \
                 nor `lane…`"
            )));
        }
        if self.permanent_lane(name).is_some() {
            return Err(Error::Other(format!("there is already a permanent lane called {name}")));
        }
        let parent = from.filter(|lane| !lane.is_main());
        if let Some(parent) = parent {
            if !self.lanes.get(&parent).is_some_and(|at| at.record.is_permanent()) {
                return Err(Error::Other(format!(
                    "{parent} is not a permanent lane: a permanent lane starts from main or another one"
                )));
            }
        }
        let author = self.identity.author().name;
        let intent = format!("the permanent lane {name}");
        self.open_lane_with(&author, None, None, &intent, &[], parent, None, Some(name.to_owned()))
    }

    /// The git branches of the permanent lanes that have a commit there.
    pub fn permanent_branches(&self) -> Vec<String> {
        self.lanes
            .values()
            .filter_map(|at| at.record.name.as_ref().map(|name| (at.record.id, name)))
            .filter(|(id, _)| self.store.lane_checkpoints(*id).any(|ck| ck.git_commit.is_some()))
            .map(|(_, name)| {
                self.config.lanes.permanent.get(name).and_then(|c| c.branch.clone()).unwrap_or(name.clone())
            })
            .collect()
    }

    /// The permanent lane of that name.
    pub fn permanent_lane(&self, name: &str) -> Option<LaneId> {
        self.lanes.values().find(|at| at.record.name.as_deref() == Some(name)).map(|at| at.record.id)
    }

    /// Refuses a person a permanent lane's `integrate` list leaves out.
    /// `person`: who asks, when a server established it; on the machine of
    /// the repository, whoever can reach it.
    pub fn may_land_on(&self, target: LaneId, person: Option<&str>) -> Result<()> {
        let (Some(person), Some(name)) =
            (person, self.lanes.get(&target).and_then(|at| at.record.name.as_deref()))
        else {
            return Ok(());
        };
        let allowed =
            self.config.lanes.permanent.get(name).map(|lane| lane.integrate.clone()).unwrap_or_default();
        match allowed.is_empty() || allowed.iter().any(|one| one == person) {
            true => Ok(()),
            false => Err(Error::Refused(format!("work lands on {name} only by {}", allowed.join(", ")))),
        }
    }

    /// Whether the lane is a permanent one (the main lane is).
    pub fn is_permanent(&self, lane: LaneId) -> bool {
        lane.is_main() || self.lanes.get(&lane).is_some_and(|at| at.record.is_permanent())
    }

    #[allow(clippy::too_many_arguments)]
    fn open_lane_with(
        &mut self,
        agent: &str,
        task: Option<String>,
        base: Option<&str>,
        intent: &str,
        scope: &[String],
        parent: Option<LaneId>,
        target: Option<LaneId>,
        name: Option<String>,
    ) -> Result<LaneInfo> {
        let agent = agent.trim();
        if agent.is_empty() {
            return Err(Error::Other("a lane needs the name of the agent that works in it".into()));
        }
        if intent.trim().is_empty() {
            return Err(Error::Other("a lane needs an intent: one sentence saying why it is opened".into()));
        }
        // A permanent lane's intent is Kito's own.
        if let Some(why) = says_nothing(intent).filter(|_| name.is_none()) {
            return Err(Error::Other(why));
        }
        // A lane for a task of this repository (ADR 0015) is for one there is, still to do.
        if let Some(n) =
            task.as_deref().filter(|task| task.trim().starts_with("task#")).and_then(crate::tasks::number)
        {
            let all = crate::tasks::load(&self.root);
            match all.get(n) {
                None => return Err(Error::Other(format!("task#{n}: no such task (`kito task list`)"))),
                Some(found) if found.state != "open" => {
                    return Err(Error::Other(format!("task#{n} is {}: reopen it first", found.state)))
                }
                Some(_) => {}
            }
        }
        // A permanent lane has no scope: it holds whatever lands on it.
        if name.is_none() && scope.iter().all(|item| item.trim().is_empty()) {
            return Err(Error::Other(
                "a lane needs a scope: what it will change, as functions (src/auth.ts:getUser), files \
                 (src/auth.ts) or directories (src/billing/)"
                    .into(),
            ));
        }
        self.scan()?;
        let main_head = self.main.head.ok_or_else(|| {
            Error::Other("the main lane is empty: there is nothing a lane could start from".into())
        })?;
        // The starting point, and the checkpoint it is filed under.
        let latest = self.store.checkpoints().next_back().map(|checkpoint| checkpoint.id);
        let (base, base_head) = match (parent, base) {
            // On top of a lane: as it is now, everything it saved included,
            // filed under the checkpoint the lane is.
            (Some(parent), _) => {
                self.scan_lane(parent)?;
                let at = self.lanes.get(&parent).ok_or(Error::NoSuchLane(parent))?;
                (at.record.base, at.work.head.unwrap_or(at.record.base_head))
            }
            (None, Some(spec)) => {
                let id: CheckpointId =
                    spec.trim().parse().map_err(|_| Error::Other(format!("{spec:?} is not a checkpoint")))?;
                let checkpoint = self
                    .store
                    .checkpoint(id)
                    .filter(|checkpoint| checkpoint.lane_of().is_main())
                    .ok_or_else(|| {
                        Error::Other(format!("{id}: this checkpoint does not exist on the main lane"))
                    })?;
                (id, checkpoint.head)
            }
            (None, None) => match latest {
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
                        approved_by: None,
                        reviewed_by: Vec::new(),
                        git_merge: None,
                        lane: None,
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
        let scope = match name {
            Some(_) => Vec::new(),
            None => self.read_scope(&base_tree, &base_symbols, &[], scope)?,
        };

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
            scope,
            // Whoever runs the repository, until told otherwise.
            owner: Some(self.identity.author()),
            proposed: None,
            git_tip: None,
            parent,
            target,
            name,
            assistant: None,
        };
        self.store.put_lane(&record)?;
        self.lanes.insert(id, LaneAt { record, work, base_tree, base_symbols });
        self.set_intent_given(id, intent, None, None)?;
        self.lane_info(id)
    }

    /// Records the last changes of a lane and closes it: its working copy
    /// goes away, its ops stay in the log.
    pub fn close_lane(&mut self, lane: LaneId) -> Result<LaneInfo> {
        if lane.is_main() {
            return Err(Error::Other("the main lane cannot be closed".into()));
        }
        if let Some(name) = self.lanes.get(&lane).and_then(|at| at.record.name.clone()) {
            return Err(Error::Other(format!("{name} is a permanent lane: it is not closed")));
        }
        self.scan_lane(lane)?;
        // The lanes on top of it go where its work went, if it went there.
        let (onto, children) = match self.lanes.get(&lane) {
            Some(at) => (at.record.target_lane(), self.children(lane)),
            None => return Err(Error::NoSuchLane(lane)),
        };
        for child in &children {
            let base_head = self.lanes[child].record.base_head;
            if !self.lane_holds(onto, base_head)? {
                return Err(Error::Refused(format!(
                    "{child} is built on work of {lane} that has not landed on {}: integrate {lane} first, or \
                     close {child}",
                    lane_word(onto)
                )));
            }
        }
        for child in children {
            let at = self.lanes.get_mut(&child).ok_or(Error::NoSuchLane(child))?;
            at.record.parent = Some(onto).filter(|onto| !onto.is_main());
            if at.record.target.is_some_and(|target| target == onto) {
                at.record.target = None;
            }
            let state = at.record.state;
            self.put_lane_state(child, state)?;
        }
        self.put_lane_state(lane, LaneState::Closed)?;
        let info = self.lane_info(lane)?;
        self.lanes.remove(&lane);
        self.claims_cache.lock().unwrap_or_else(|e| e.into_inner()).remove(&lane);
        self.told.remove(&lane);
        self.reasons_told.remove(&lane);
        let dir = self.lane_dir(lane);
        kito_lane::remove(&dir).map_err(Error::io(&dir))?;
        Ok(LaneInfo { workdir: None, ..info })
    }

    /// How many ops a lane has recorded: it grows with every change.
    pub fn lane_ops(&self, lane: LaneId) -> Result<u64> {
        Ok(self.work(lane)?.ops)
    }

    /// Says whose lane it is: the person a client was authenticated as.
    pub fn set_lane_owner(&mut self, lane: LaneId, owner: kito_core::Author) -> Result<()> {
        let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
        at.record.owner = Some(owner);
        let state = at.record.state;
        self.put_lane_state(lane, state)
    }

    /// Takes in what is said of the assistant at work in a lane (ADR
    /// 0010). The lane is written again only when it says something new.
    pub fn lane_assistant_said(&mut self, lane: LaneId, said: kito_core::AssistantSaid) -> Result<()> {
        let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
        if !kito_core::Assistant::told(&mut at.record.assistant, said) {
            return Ok(());
        }
        let state = at.record.state;
        self.put_lane_state(lane, state)
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

    /// Declares the "why" of the ops a lane records from now on. An intent
    /// that says nothing (`wip`, `fix`, `update`) is refused: it is what the
    /// next agent will be told before it changes this code.
    pub fn set_intent(
        &mut self,
        lane: LaneId,
        summary: &str,
        task: Option<String>,
        prompt: Option<&str>,
    ) -> Result<String> {
        if let Some(why) = says_nothing(summary) {
            return Err(Error::Other(why));
        }
        self.set_intent_given(lane, summary, task, prompt)
    }

    /// The same, for an intent Kito writes itself (a git commit's subject,
    /// an integration): taken as it is.
    pub(crate) fn set_intent_given(
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
            state: match &record.proposed {
                Some(_) => "awaiting_approval".to_owned(),
                None => state_word(record.state).to_owned(),
            },
            workdir,
            ops,
            files,
            conflicts: self
                .store
                .conflicts()
                .filter(|c| c.assigned_to == Some(record.id) && c.resolved_by.is_none())
                .map(|c| c.id.to_string())
                .collect(),
            scope: record.scope.iter().map(|entry| crate::scope::show(&entry.item)).collect(),
            out_of_scope: match self.lanes.contains_key(&record.id) {
                true => self.out_of_scope(record.id).unwrap_or_default(),
                false => Vec::new(),
            },
            owner: record.owner.as_ref().map(|owner| owner.name.clone()),
            proposed_by: record.proposed.as_ref().map(|proposal| proposal.by.name.clone()),
            parent: record.parent.map(|lane| self.lane_name(lane)),
            target: (record.parent.is_some() || record.target.is_some())
                .then(|| self.lane_name(record.target_lane())),
            name: record.name.clone(),
            review: match self.lanes.contains_key(&record.id) && record.name.is_none() {
                true => {
                    let (missing, approvers) = self.review_gate(record.id);
                    let names: Vec<&str> = approvers.iter().map(|person| person.name.as_str()).collect();
                    missing
                        .or_else(|| (!names.is_empty()).then(|| format!("approved by {}", names.join(", "))))
                }
                false => None,
            },
            assistant: record.assistant.clone(),
        }
    }

    /// Whether the lane `lane` already has the op `op` in its history:
    /// recorded on it, or taken in by one of its ops (through both
    /// parents). Ops are older than those made on top of them, so the walk
    /// stops at ops older than `op`.
    pub(crate) fn lane_holds(&self, lane: LaneId, op: OpId) -> Result<bool> {
        let Some(head) = self.work(lane)?.head else { return Ok(false) };
        let wanted = match self.store.get_op(&op)? {
            Some(found) => found,
            None => return Ok(false),
        };
        let mut seen = std::collections::HashSet::new();
        let mut todo = vec![head];
        while let Some(at) = todo.pop() {
            if at == op {
                return Ok(true);
            }
            if !seen.insert(at) {
                continue;
            }
            let Some(found) = self.store.get_op(&at)? else { continue };
            if found.at < wanted.at {
                continue;
            }
            todo.extend(found.parents.iter().copied());
        }
        Ok(false)
    }

    /// Why `lane` cannot land on its target yet: the lane whose work it was
    /// built on and that the target does not have.
    pub(crate) fn waits_for(&self, lane: LaneId) -> Result<Option<LaneId>> {
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let target = at.record.target_lane();
        if target == at.record.parent_lane() || self.lane_holds(target, at.record.base_head)? {
            return Ok(None);
        }
        Ok(Some(at.record.parent_lane()))
    }

    /// A lane is changed by the person it was opened for, or by somebody
    /// who may integrate. `person`: who asks, when a server established it;
    /// on the machine of the repository, whoever can reach it.
    pub fn may_change(&self, lane: LaneId, person: Option<&str>, integrator: bool) -> Result<()> {
        let (Some(person), false) = (person, integrator) else { return Ok(()) };
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        match at.record.owner.as_ref().map(|owner| owner.name.as_str()) {
            Some(owner) if owner != person => Err(Error::Refused(format!(
                "{lane} is {owner}'s: it is changed by its owner, or by somebody who may integrate"
            ))),
            _ => Ok(()),
        }
    }

    pub fn lane_info(&self, lane: LaneId) -> Result<LaneInfo> {
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let mut record = at.record.clone();
        record.intent = at.work.intent;
        // What was approved for waiting is the lane as it was then.
        record.proposed = record.proposed.filter(|proposal| proposal.ops == at.work.ops);
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
