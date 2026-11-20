//! Why code is the way it is.
//!
//! Every op Kito records carries a reason: the intent its lane declared,
//! or, for what was put in the main directory by hand or came from git,
//! the checkpoint that took it in. This module turns that into something
//! to ask before changing code somebody else wrote: for a function, a file
//! or a few lines, the reasons it changed (who, for whom, in which lane,
//! which checkpoint, which checks), what else changed for the same reason,
//! who uses it, who is on it now, and which reason each of its lines was
//! last written for. And the other way round: which code a reason touched.
//!
//! Nothing new is stored. The answers are read from the log; an index from
//! reasons to what they touched is built in memory the first time somebody
//! asks, and kept up to date as ops are recorded.

use crate::error::{Error, Result};
use crate::repo::Repo;
use kito_core::{CheckpointId, Hlc, IntentId, LaneId, Op, OpId, OpKind, SymbolId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// The intent the first reading of a file's symbols is recorded with: it
/// says how Kito came to know a function, not why the function is there.
const ANALYSIS: &str = "first analysis of the symbols";

/// How many neighbours, co-changes and hits an answer lists.
const LISTED: usize = 12;

/// Above this many files the uses of a symbol are not looked for unless
/// asked: the syntax is read file by file.
const USES_LOOKED_FOR_UP_TO: usize = 5_000;

/// A reason as the log has it: the intent an op was recorded with, in the
/// lane it was recorded in. Ops of the main lane that carry none are told
/// apart by the checkpoint that took them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Key {
    lane: LaneId,
    intent: Option<IntentId>,
    checkpoint: Option<CheckpointId>,
}

/// What was changed for one reason.
#[derive(Clone, Debug, Default)]
struct Touched {
    symbols: BTreeSet<SymbolId>,
    files: BTreeSet<PathBuf>,
    first: Hlc,
    last: Hlc,
    authors: BTreeSet<String>,
}

/// A change to a file as a whole, or to its text outside every symbol.
#[derive(Clone, Debug)]
struct FileTouch {
    how: &'static str,
    op: OpId,
}

/// A checkpoint of the main lane, with what is needed to place an op in it.
#[derive(Clone, Debug)]
struct Ck {
    id: CheckpointId,
    at: Hlc,
    /// The lane it integrated, if it is an integration.
    lane: Option<LaneId>,
}

/// From reasons to what they touched, and from files to the reasons that
/// touched them as a whole.
#[derive(Default)]
pub(crate) struct WhyIndex {
    /// How many ops of the log are in it.
    ops: u64,
    touched: HashMap<Key, Touched>,
    files: HashMap<PathBuf, Vec<FileTouch>>,
    checkpoints: Vec<Ck>,
    /// Ops of the main lane with no intent that no checkpoint has taken in
    /// yet: when one does, they are its.
    loose: Vec<OpId>,
    /// How many times it was built from the whole log.
    builds: u32,
}

/// The lane an integration intent names: `integrate lane/3: prices in cents`.
fn integrated_lane(summary: &str) -> Option<LaneId> {
    summary.strip_prefix("integrate ")?.split([':', ' ']).next()?.parse().ok()
}

impl WhyIndex {
    fn checkpoint_at(&self, at: Hlc) -> Option<&Ck> {
        let first = self.checkpoints.partition_point(|ck| ck.at < at);
        self.checkpoints.get(first)
    }

    /// The first integration of `lane` at or after `at`.
    fn integration_of(&self, lane: LaneId, at: Hlc) -> Option<&Ck> {
        let first = self.checkpoints.partition_point(|ck| ck.at < at);
        self.checkpoints[first..].iter().find(|ck| ck.lane == Some(lane))
    }

    fn key_of(&self, op: &Op) -> Key {
        let checkpoint = match (op.lane.is_main(), op.intent) {
            (true, None) => self.checkpoint_at(op.at).map(|ck| ck.id),
            _ => None,
        };
        Key { lane: op.lane, intent: op.intent, checkpoint }
    }

    /// Files an op under the reason it was made for.
    fn file(&mut self, op: &Op) {
        let key = self.key_of(op);
        if key.lane.is_main() && key.intent.is_none() && key.checkpoint.is_none() {
            self.loose.push(op.id);
        }
        let touched = self.touched.entry(key).or_default();
        if touched.authors.is_empty() {
            touched.first = op.at;
        }
        touched.last = touched.last.max(op.at);
        touched.authors.insert(op.author.name.clone());
        let file = match &op.kind {
            OpKind::TextEdit { file, .. } => Some(file),
            OpKind::FileAdd { path, .. } | OpKind::FileRemove { path } => Some(path),
            OpKind::FileRename { to, .. } => Some(to),
            kind => {
                touched.symbols.extend(kind.symbol());
                None
            }
        };
        touched.files.extend(file.cloned());
    }

    /// Takes an op in.
    pub(crate) fn add(&mut self, op: &Op) {
        self.file(op);
        let mut whole = |path: &Path, how: &'static str| {
            self.files.entry(path.to_owned()).or_default().push(FileTouch { how, op: op.id });
        };
        match &op.kind {
            OpKind::TextEdit { file, .. } => whole(file, "changed"),
            OpKind::FileAdd { path, .. } => whole(path, "created"),
            OpKind::FileRemove { path } => whole(path, "removed"),
            OpKind::FileRename { from, to } => {
                whole(from, "renamed away");
                whole(to, "renamed");
            }
            _ => {}
        }
        self.ops += 1;
    }
}

/// What was asked about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// `function`, `class`, …, or `file`.
    pub kind: String,
    pub file: String,
    /// The symbol's qualified name; `None` for a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The symbol's permanent ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The lines it is on now, from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<(u32, u32)>,
}

/// One reason a piece of code changed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    /// Why, as it was declared: "prices in cents".
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// What it did to the target: `created`, `body changed`, `renamed
    /// getUser → fetchUser`, …
    pub how: Vec<String>,
    /// Who made the change: the agent, or the person.
    pub by: String,
    /// The person the agent worked for.
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// When, in milliseconds since the Unix epoch: the last change.
    pub at_ms: u64,
    /// The checkpoint that brought it to the main lane; `None`: it is not
    /// there yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Who let it in, when a person did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    /// The checks that passed before it went in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    /// What else changed for the same reason: functions (`file:name`) and
    /// files, up to a dozen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with: Vec<String>,
    /// How many more there are.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub with_more: usize,
    /// The ops, as `kito log` names them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Lines that were last written for the same reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRun {
    pub from: u32,
    pub to: u32,
    /// An index in `reasons`; `None`: older than what the history tells.
    pub reason: Option<usize>,
}

/// A reason in one line: the last one of a neighbour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub why: String,
    pub by: String,
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    pub at_ms: u64,
}

/// A symbol next to the target: one that uses it, one it uses, one the
/// file contains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Neighbour {
    /// `file:name`.
    pub symbol: String,
    /// Where the use is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The last reason it changed for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Brief>,
}

/// A lane that is on the target now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InProgress {
    pub lane: String,
    pub agent: String,
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    pub intent: Option<String>,
    /// How it is on it: it declared it, changed it, or works on code that
    /// calls it.
    pub how: String,
}

/// A conflict on the target, and what was decided.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub conflict: String,
    pub kind: String,
    pub lane: Option<String>,
    /// `main`, `lane`, `text`, `rewrite`, `keep`; `None`: not decided yet.
    pub decided: Option<String>,
    /// An integration applied the decision.
    pub applied: bool,
}

/// The answer to "why is this the way it is".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhyGraph {
    pub target: Target,
    /// The reasons it changed, newest first.
    pub reasons: Vec<Reason>,
    /// Its lines as they are now, grouped by the reason they were last
    /// written for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<LineRun>,
    /// Who uses it, as far as the syntax tells.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_by: Vec<Neighbour>,
    /// What it uses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<Neighbour>,
    /// For a file: the functions it contains.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains: Vec<Neighbour>,
    /// The lanes that are on it now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_progress: Vec<InProgress>,
    /// The conflicts decided on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
    /// What the answer leaves out, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A reason found by its words, with what it changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasonHit {
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub by: Vec<String>,
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    pub at_ms: u64,
    /// The functions (`file:name`) and files it changed, up to a dozen.
    pub changed: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub changed_more: usize,
}

/// What is asked about, as it is written: `src/shop.ts:total`, `src/shop.ts`,
/// `src/shop.ts:12`, `src/shop.ts:12-20`.
enum Asked {
    Symbol(PathBuf, String),
    File(PathBuf),
    Lines(PathBuf, u32, u32),
}

fn parse_target(text: &str) -> Result<Asked> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Other("ask about a function (file:name), a file, or lines (file:12-20)".into()));
    }
    // The file comes first: the name of a symbol may itself have colons.
    let Some((file, rest)) = text.split_once(':').filter(|(file, rest)| !file.is_empty() && !rest.is_empty())
    else {
        return Ok(Asked::File(PathBuf::from(text)));
    };
    let number = |text: &str| text.parse::<u32>().ok().filter(|line| *line > 0);
    let lines = match rest.split_once('-') {
        Some((from, to)) => number(from).zip(number(to)),
        None => number(rest).map(|line| (line, line)),
    };
    Ok(match lines {
        Some((from, to)) => Asked::Lines(PathBuf::from(file), from.min(to), from.max(to)),
        None => Asked::Symbol(PathBuf::from(file), rest.to_owned()),
    })
}

/// For each line of the last version, the version that introduced it.
fn blame(versions: &[String]) -> Vec<usize> {
    let lines = |text: &str| -> Vec<String> { text.lines().map(str::to_owned).collect() };
    let Some(first) = versions.first() else { return Vec::new() };
    let mut old = lines(first);
    let mut from: Vec<usize> = vec![0; old.len()];
    for (index, version) in versions.iter().enumerate().skip(1) {
        let new = lines(version);
        let mut next = Vec::with_capacity(new.len());
        for op in similar::capture_diff_slices(similar::Algorithm::Myers, &old, &new) {
            let (tag, was, now) = op.as_tag_tuple();
            match tag {
                similar::DiffTag::Equal => next.extend(was.map(|line| from[line])),
                similar::DiffTag::Delete => {}
                similar::DiffTag::Insert | similar::DiffTag::Replace => next.extend(now.map(|_| index)),
            }
        }
        (old, from) = (new, next);
    }
    from
}

/// What a symbolic op did to its symbol, in a word or two.
fn how(kind: &OpKind) -> String {
    match kind {
        OpKind::AddSymbol { .. } => "created".to_owned(),
        OpKind::RemoveSymbol { .. } => "removed".to_owned(),
        OpKind::ModifyBody { .. } => "body changed".to_owned(),
        OpKind::ModifySignature { .. } => "signature changed".to_owned(),
        OpKind::Rename { from, to, .. } => format!("renamed {from} → {to}"),
        OpKind::Move { from, to, .. } if from.file != to.file => {
            format!("moved from {} to {}", from.file.display(), to.file.display())
        }
        OpKind::Move { .. } => "moved".to_owned(),
        _ => "changed".to_owned(),
    }
}

/// Where a change is: the reason it was made for, and the checkpoint that
/// brought it to the main lane (`None`: it is not there yet). What a lane
/// did before one of its integrations and after it are two places, even
/// under one intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Place {
    key: Key,
    landed: Option<CheckpointId>,
}

/// The ops made for one reason, on one target.
struct Group {
    place: Place,
    ops: Vec<Op>,
    hows: Vec<String>,
}

impl Group {
    fn last(&self) -> Hlc {
        self.ops.iter().map(|op| op.at).max().unwrap_or(Hlc::ZERO)
    }
}

/// What an answer is about: it is not named among what else changed.
#[derive(Clone, Copy)]
enum About<'a> {
    Symbol(SymbolId),
    File(&'a Path),
    Nothing,
}

/// The lines of a target: where each was written, the number of the first
/// in its file, and the ones that were asked about (`None`: all).
struct Written<'a> {
    lines: &'a [Option<Place>],
    first: u32,
    asked: Option<(u32, u32)>,
}

/// Who uses a symbol, with the line of the use, and what it uses.
type Neighbours = (Vec<(SymbolId, u32)>, Vec<SymbolId>);

/// Adds an op to the group of its place, or starts one.
fn gather(groups: &mut Vec<Group>, place: Place, op: Op, how: String) {
    match groups.iter_mut().find(|group| group.place == place) {
        Some(group) => {
            group.ops.push(op);
            group.hows.push(how);
        }
        None => groups.push(Group { place, ops: vec![op], hows: vec![how] }),
    }
}

/// How many versions of a file are read to tell who wrote its lines: what
/// is older than that is told as one.
const VERSIONS_READ: usize = 400;

impl WhyIndex {
    fn place_of(&self, op: &Op) -> Place {
        let landed = match op.lane.is_main() {
            true => self.checkpoint_at(op.at),
            false => self.integration_of(op.lane, op.at),
        };
        Place { key: self.key_of(op), landed: landed.map(|ck| ck.id) }
    }

    /// When a place got to the main lane.
    fn landed_at(&self, place: &Place) -> Option<Hlc> {
        let id = place.landed?;
        self.checkpoints.iter().find(|ck| ck.id == id).map(|ck| ck.at)
    }
}

impl Repo {
    /// The index of reasons, built if nobody asked before and brought up
    /// to what the log has.
    fn why_index(&mut self) -> Result<&WhyIndex> {
        let ops = self.store.op_count()?;
        let checkpoints = self.store.checkpoints().count();
        if !self.why_index.as_ref().is_some_and(|index| index.ops == ops) {
            // What is still in memory goes to the log first: the log is
            // what is read.
            self.store.commit()?;
            let builds = self.why_index.as_ref().map_or(0, |index| index.builds);
            let mut index = WhyIndex { builds: builds + 1, ..WhyIndex::default() };
            self.why_checkpoints(&mut index)?;
            // One op at a time: a long history is never held whole.
            self.store.scan_ops(|_, op| {
                index.add(&op);
                Ok(())
            })?;
            // An op the log holds twice is one op.
            index.ops = self.store.op_count()?;
            self.why_index = Some(index);
        } else if self.why_index.as_ref().is_some_and(|index| index.checkpoints.len() != checkpoints) {
            let mut index = self.why_index.take().unwrap_or_default();
            self.why_checkpoints(&mut index)?;
            // What was put in the main directory by hand since the last
            // checkpoint belongs to the one that took it in: filed again.
            let loose = std::mem::take(&mut index.loose);
            if !loose.is_empty() {
                index.touched.remove(&Key { lane: LaneId::MAIN, intent: None, checkpoint: None });
                for id in loose {
                    if let Some(op) = self.store.get_op(&id)? {
                        index.file(&op);
                    }
                }
            }
            self.why_index = Some(index);
        }
        self.index()
    }

    /// How many times the index of reasons was built from the whole log:
    /// once, unless something recorded ops behind its back.
    pub fn why_index_builds(&self) -> u32 {
        self.why_index.as_ref().map_or(0, |index| index.builds)
    }

    fn index(&self) -> Result<&WhyIndex> {
        self.why_index.as_ref().ok_or_else(|| Error::Other("the index of reasons is not there".into()))
    }

    /// Brings the checkpoints of an index up to the store's.
    fn why_checkpoints(&self, index: &mut WhyIndex) -> Result<()> {
        for checkpoint in self.store.checkpoints().skip(index.checkpoints.len()) {
            let head = self.store.get_op(&checkpoint.head)?;
            let lane = head
                .as_ref()
                .and_then(|op| op.intent)
                .and_then(|intent| self.store.intent(&intent))
                .and_then(|intent| integrated_lane(&intent.summary));
            index.checkpoints.push(Ck { id: checkpoint.id, at: head.map_or(Hlc::ZERO, |op| op.at), lane });
        }
        Ok(())
    }

    /// The words of an op's intent.
    fn words(&self, op: &Op) -> Option<&str> {
        op.intent.and_then(|intent| self.store.intent(&intent)).map(|intent| intent.summary.as_str())
    }

    /// The words of a reason, and its task.
    fn reason_words(&self, key: &Key) -> (String, Option<String>) {
        if let Some(intent) = key.intent.and_then(|id| self.store.intent(&id)) {
            // The task is the intent's, or the one its lane was opened for.
            let lane = self.store.lane_record(key.lane).filter(|_| !key.lane.is_main());
            let task = intent.task.clone().or_else(|| lane.and_then(|lane| lane.task.clone()));
            return (intent.summary.clone(), task);
        }
        match key.checkpoint.and_then(|id| self.store.checkpoint(id)) {
            Some(checkpoint) => (checkpoint.label.clone(), None),
            None => ("(no reason was recorded: an edit made by hand, in no checkpoint yet)".to_owned(), None),
        }
    }

    /// `file:name` for a symbol, as the lane calls it now or as it was
    /// last called.
    fn symbol_label(&self, lane: LaneId, id: SymbolId) -> String {
        let table = self.work(lane).map(|work| &work.symbols).unwrap_or(&self.main.symbols);
        match table.get(id) {
            Some(entry) => {
                let name = table.qualified_name(id).unwrap_or_else(|| entry.name.clone());
                format!("{}:{name}", entry.file.display())
            }
            None => {
                let name =
                    crate::semantic::last_known_name(&self.store, id).unwrap_or_else(|| id.to_string());
                format!("{name} (no longer there)")
            }
        }
    }

    /// `true` for what Kito recorded of its own doing: an integration that
    /// says again what its lane said, or the first reading of a file.
    fn kitos_own(&self, op: &Op, told: &BTreeSet<LaneId>) -> bool {
        match self.words(op) {
            Some(ANALYSIS) => true,
            Some(words) => {
                op.lane.is_main() && integrated_lane(words).is_some_and(|lane| told.contains(&lane))
            }
            None => false,
        }
    }

    /// The ops of a symbol that a lane has, grouped by the reason they
    /// were made for and by the integration that brought them.
    fn symbol_groups(&self, reach: &crate::semantic::Reach, id: SymbolId) -> Result<Vec<Group>> {
        let index = self.index()?;
        let events: Vec<Op> = crate::semantic::timeline(&self.store, id, 0)?
            .into_iter()
            .map(|(op, _)| op)
            .filter(|op| reach.includes(op))
            .collect();
        // The lanes whose own ops are in the story: their integrations add nothing.
        let told: BTreeSet<LaneId> =
            events.iter().filter(|op| !op.lane.is_main()).map(|op| op.lane).collect();
        let mut groups: Vec<Group> = Vec::new();
        for op in events {
            if !self.kitos_own(&op, &told) {
                let (place, how) = (index.place_of(&op), how(&op.kind));
                gather(&mut groups, place, op, how);
            }
        }
        Ok(groups)
    }

    /// The same for a file as a whole: every change to it, inside its
    /// functions or not.
    fn file_groups(&self, reach: &crate::semantic::Reach, file: &Path) -> Result<Vec<Group>> {
        let index = self.index()?;
        let mut touches: Vec<(Op, &'static str)> = Vec::new();
        for touch in index.files.get(file).map(Vec::as_slice).unwrap_or_default() {
            if let Some(op) = self.store.get_op(&touch.op)?.filter(|op| reach.includes(op)) {
                touches.push((op, touch.how));
            }
        }
        let told: BTreeSet<LaneId> =
            touches.iter().filter(|(op, _)| !op.lane.is_main()).map(|(op, _)| op.lane).collect();
        let mut groups: Vec<Group> = Vec::new();
        for (op, how) in touches {
            if !self.kitos_own(&op, &told) {
                let place = index.place_of(&op);
                gather(&mut groups, place, op, how.to_owned());
            }
        }
        Ok(groups)
    }

    /// Groups in the order they got where the asker is: what a lane did
    /// counts from the integration that brought it, and what is not
    /// integrated yet (the asking lane's own work) is the newest.
    fn in_order(&self, groups: &mut [Group]) {
        let Ok(index) = self.index() else { return };
        groups.sort_by_key(|group| {
            let landed = match group.place.key.lane.is_main() {
                true => Some(group.last()),
                false => index.landed_at(&group.place),
            };
            (landed.is_none(), landed, group.last())
        });
    }

    /// A group of ops as a reason.
    fn reason_of(&self, view: LaneId, group: &Group, about: About<'_>) -> Reason {
        let Ok(index) = self.index() else { return Reason::default() };
        let key = &group.place.key;
        let (why, task) = self.reason_words(key);
        let checkpoint = group.place.landed.and_then(|id| self.store.checkpoint(id));
        let record = self.store.lane_record(key.lane);
        let (with, with_more) = match index.touched.get(key) {
            Some(touched) => self.touched_names(view, touched, about),
            None => (Vec::new(), 0),
        };
        let mut how = group.hows.clone();
        how.dedup();
        Reason {
            why,
            task,
            how,
            by: group.ops.first().map(|op| op.author.name.clone()).unwrap_or_default(),
            person: record.and_then(|lane| lane.owner.as_ref()).map(|owner| owner.name.clone()),
            lane: (!key.lane.is_main()).then(|| key.lane.to_string()),
            at_ms: group.last().wall_ms,
            checkpoint: checkpoint.map(|ck| ck.id.to_string()),
            approved_by: checkpoint.and_then(|ck| ck.approved_by.as_ref()).map(|person| person.name.clone()),
            checks: checkpoint
                .and_then(|ck| ck.verification.as_ref())
                .filter(|verification| verification.passed)
                .map(|verification| verification.commands.clone())
                .unwrap_or_default(),
            with,
            with_more,
            ops: group.ops.iter().map(|op| op.id.to_string().chars().take(8).collect()).collect(),
        }
    }

    /// What a reason touched, by name: functions as `file:name`, and the
    /// files none of them is in. Up to a dozen; the number says how many
    /// more there are. `about`: the one that is being asked about, and so
    /// is not "something else".
    fn touched_names(&self, view: LaneId, touched: &Touched, about: About<'_>) -> (Vec<String>, usize) {
        // The one that is asked about is not "something else", nor is its file.
        let (about, asked_file) = match about {
            About::Symbol(id) => (Some(id), None),
            About::File(file) => (None, Some(file.display().to_string())),
            About::Nothing => (None, None),
        };
        let symbols = touched.symbols.iter().filter(|id| Some(**id) != about);
        // A reason that touched thousands of things (a first import) is
        // not named whole to list a dozen.
        if touched.symbols.len() + touched.files.len() > 20 * LISTED {
            let names: Vec<String> = symbols.take(LISTED).map(|id| self.symbol_label(view, *id)).collect();
            let all =
                touched.symbols.len() - usize::from(about.is_some_and(|id| touched.symbols.contains(&id)));
            return (names, all.saturating_sub(LISTED));
        }
        let mut names: Vec<String> = symbols.map(|id| self.symbol_label(view, *id)).collect();
        // A file is named when none of its functions is: the target's own file never.
        let own = about
            .and_then(|id| {
                self.work(view).ok()?.symbols.get(id).map(|entry| entry.file.display().to_string())
            })
            .or(asked_file);
        let named: BTreeSet<String> =
            names.iter().filter_map(|label| label.split(':').next()).map(str::to_owned).chain(own).collect();
        names.extend(
            touched.files.iter().map(|path| path.display().to_string()).filter(|path| !named.contains(path)),
        );
        names.sort();
        names.dedup();
        let more = names.len().saturating_sub(LISTED);
        names.truncate(LISTED);
        (names, more)
    }

    /// The last reason a symbol changed for, in one line.
    fn brief(&self, lane: LaneId, reach: &crate::semantic::Reach, id: SymbolId) -> Option<Brief> {
        let mut groups = self.symbol_groups(reach, id).ok()?;
        // A function nobody changed since Kito first read its file: the
        // file's last reason is the nearest thing to its own.
        if groups.is_empty() {
            let file = self.work(lane).ok()?.symbols.get(id)?.file.clone();
            groups = self.file_groups(reach, &file).ok()?;
        }
        self.in_order(&mut groups);
        let reason = self.reason_of(lane, groups.last()?, About::Symbol(id));
        Some(Brief {
            why: reason.why,
            by: reason.by,
            person: reason.person,
            checkpoint: reason.checkpoint,
            at_ms: reason.at_ms,
        })
    }

    /// The content of a file after each of some ops on it, oldest first.
    /// A file that was renamed starts from what it held then.
    fn file_versions(&self, ops: &[&Op]) -> Vec<(OpId, String)> {
        let text = |blob: &kito_core::BlobId| -> Option<String> {
            self.store.get_blob(blob).ok().flatten().map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };
        let mut versions: Vec<(OpId, String)> = Vec::new();
        for op in ops {
            match &op.kind {
                OpKind::FileAdd { blob, .. } => versions.extend(text(blob).map(|content| (op.id, content))),
                OpKind::TextEdit { before, after, .. } => {
                    // Its history under another name is not followed: what
                    // it held when it got this one is where the story starts.
                    if versions.is_empty() {
                        versions.extend(text(before).map(|content| (OpId::from_bytes([0; 32]), content)));
                    }
                    versions.extend(text(after).map(|content| (op.id, content)));
                }
                OpKind::FileRemove { .. } => versions.push((op.id, String::new())),
                _ => {}
            }
        }
        versions
    }

    /// For each line of a file as a lane has it, where it was last written:
    /// the reason, and the integration that brought it. `own`: the lane's
    /// work since it last started from the main lane is part of the story;
    /// without it, `now` is the file as the lane found it.
    fn file_lines(
        &self,
        view: LaneId,
        reach: &crate::semantic::Reach,
        file: &Path,
        now: &str,
        own: bool,
    ) -> Result<Vec<Option<Place>>> {
        let index = self.index()?;
        let mut all: Vec<Op> = Vec::new();
        for touch in index.files.get(file).map(Vec::as_slice).unwrap_or_default() {
            all.extend(self.store.get_op(&touch.op)?);
        }
        // The chain the lane's own text went through: its ops since it
        // last started from its parent, the parent's before that and since
        // it started from its own, and so on down to the main lane's (ADR
        // 0001). Each piece: a lane, and the times its ops are between.
        let mut pieces: Vec<(LaneId, Option<Hlc>, Option<Hlc>)> = Vec::new();
        let (mut lane, mut upper, mut first) = (view, None, true);
        while pieces.len() < 64 {
            if lane.is_main() {
                pieces.push((LaneId::MAIN, None, upper));
                break;
            }
            let record = self.store.lane_record(lane);
            let based =
                record.and_then(|record| self.store.get_op(&record.base_head).ok().flatten()).map(|op| op.at);
            if !first || own {
                pieces.push((lane, based, upper));
            }
            first = false;
            upper = based;
            lane = record.map_or(LaneId::MAIN, |record| record.parent_lane());
        }
        let mut chain: Vec<&Op> = all
            .iter()
            .filter(|op| {
                pieces.iter().any(|(lane, after, until)| {
                    op.lane == *lane
                        && after.is_none_or(|after| op.at > after)
                        && until.is_none_or(|until| op.at <= until)
                })
            })
            .filter(|op| reach.includes(op))
            .collect();
        if chain.len() > VERSIONS_READ {
            chain.drain(..chain.len() - VERSIONS_READ);
        }
        let mut versions = self.file_versions(&chain);
        let known = versions.len();
        if versions.last().map(|(_, text)| text.as_str()) != Some(now) {
            versions.push((OpId::from_bytes([0; 32]), now.to_owned()));
        }
        let texts: Vec<String> = versions.iter().map(|(_, text)| text.clone()).collect();
        let written = blame(&texts);
        let mut lines: Vec<Option<OpId>> =
            written.iter().map(|version| (*version < known).then(|| versions[*version].0)).collect();

        // What an integration wrote was written in its lane, line by line.
        let integrations: Vec<(OpId, LaneId, Hlc)> = chain
            .iter()
            .filter(|op| op.lane.is_main())
            .filter_map(|op| Some((op.id, integrated_lane(self.words(op)?)?, op.at)))
            .collect();
        let now_lines: Vec<&str> = now.lines().collect();
        for (position, (merge, lane, at)) in integrations.iter().enumerate() {
            if !lines.contains(&Some(*merge)) {
                continue;
            }
            // The lane's ops on the file since its integration before.
            let since =
                integrations[..position].iter().rev().find(|(_, l, _)| l == lane).map(|(_, _, at)| *at);
            let theirs: Vec<&Op> = all
                .iter()
                .filter(|op| op.lane == *lane && op.at <= *at && since.is_none_or(|since| op.at > since))
                .collect();
            let lane_versions = self.file_versions(&theirs);
            let Some(merged_at) = versions.iter().position(|(op, _)| op == merge) else { continue };
            let Some((_, last)) = lane_versions.last() else { continue };
            // Which lane op wrote each line of the lane's last version.
            let base = (merged_at > 0).then(|| versions[merged_at - 1].1.clone());
            let offset = usize::from(base.is_some());
            let texts: Vec<String> =
                base.into_iter().chain(lane_versions.iter().map(|(_, text)| text.clone())).collect();
            let mut by_text: HashMap<&str, OpId> = HashMap::new();
            for (line, version) in last.lines().zip(blame(&texts)) {
                if version >= offset {
                    by_text.entry(line).or_insert(lane_versions[version - offset].0);
                }
            }
            for (line, op) in lines.iter_mut().enumerate() {
                if *op == Some(*merge) {
                    if let Some(lane_op) = now_lines.get(line).and_then(|text| by_text.get(text)) {
                        *op = Some(*lane_op);
                    }
                }
            }
        }
        // A lane that landed on another lane (ADR 0001): what the landing
        // wrote on that lane was written in the lane that landed, and so on
        // down the lanes stacked on each other.
        for _ in 0..8 {
            let landings: Vec<(&Op, LaneId)> = all
                .iter()
                .filter(|op| !op.lane.is_main() && lines.contains(&Some(op.id)))
                .filter_map(|op| Some((op, integrated_lane(self.words(op)?)?)))
                .collect();
            if landings.is_empty() {
                break;
            }
            for (landing, lane) in landings {
                let onto = landing.lane;
                // The lane's ops since it last landed on the same lane.
                let since = all
                    .iter()
                    .filter(|op| op.lane == onto && op.at < landing.at)
                    .filter(|op| self.words(op).and_then(integrated_lane) == Some(lane))
                    .map(|op| op.at)
                    .max();
                let theirs: Vec<&Op> = all
                    .iter()
                    .filter(|op| {
                        op.lane == lane && op.at <= landing.at && since.is_none_or(|since| op.at > since)
                    })
                    .collect();
                let lane_versions = self.file_versions(&theirs);
                let Some((_, last)) = lane_versions.last() else { continue };
                // The file as the lane landed on had it before.
                let before: Vec<&Op> =
                    all.iter().filter(|op| op.lane == onto && op.at < landing.at).collect();
                let base = self.file_versions(&before).last().map(|(_, text)| text.clone());
                let offset = usize::from(base.is_some());
                let texts: Vec<String> =
                    base.into_iter().chain(lane_versions.iter().map(|(_, text)| text.clone())).collect();
                let mut by_text: HashMap<&str, OpId> = HashMap::new();
                for (line, version) in last.lines().zip(blame(&texts)) {
                    if version >= offset {
                        by_text.entry(line).or_insert(lane_versions[version - offset].0);
                    }
                }
                // A line that cannot be followed stays with the landing.
                for (line, op) in lines.iter_mut().enumerate() {
                    if *op == Some(landing.id) {
                        if let Some(lane_op) = now_lines.get(line).and_then(|text| by_text.get(text)) {
                            *op = Some(*lane_op);
                        }
                    }
                }
            }
        }
        let places: HashMap<OpId, Place> = all.iter().map(|op| (op.id, index.place_of(op))).collect();
        Ok(lines.into_iter().map(|op| op.and_then(|op| places.get(&op).copied())).collect())
    }

    /// The reasons of a target, newest first, and its lines by reason.
    /// `groups`: the ops on the target; `lines`: for each of its lines (the
    /// first being line `first` of the file), where it was written. A line
    /// written for a reason that is not among the groups brings it in:
    /// `more` has the ops of the whole file.
    fn answer(
        &self,
        view: LaneId,
        mut groups: Vec<Group>,
        more: Vec<Group>,
        written: Written<'_>,
        about: About<'_>,
    ) -> (Vec<Reason>, Vec<LineRun>) {
        let Written { lines, first, asked } = written;
        let shown = |line: u32| asked.is_none_or(|(from, to)| from <= line && line <= to);
        for mut group in more {
            let owns = lines
                .iter()
                .enumerate()
                .any(|(offset, place)| *place == Some(group.place) && shown(first + offset as u32));
            if owns && !groups.iter().any(|known| known.place == group.place) {
                // Of what it did to the file, this much is known: these lines.
                group.hows = vec!["wrote lines of it".to_owned()];
                groups.push(group);
            }
        }
        self.in_order(&mut groups);
        let newest = groups.len().saturating_sub(1);
        let mut runs: Vec<LineRun> = Vec::new();
        for (offset, place) in lines.iter().enumerate() {
            let line = first + offset as u32;
            if !shown(line) {
                continue;
            }
            let reason = place
                .and_then(|place| groups.iter().position(|group| group.place == place))
                .map(|position| newest - position);
            match runs.last_mut() {
                Some(run) if run.reason == reason && run.to + 1 == line => run.to = line,
                _ => runs.push(LineRun { from: line, to: line, reason }),
            }
        }
        let reasons = groups.iter().rev().map(|group| self.reason_of(view, group, about)).collect();
        (reasons, runs)
    }

    /// Why a function, a file or some lines are the way they are, as
    /// `lane` has them. `uses`: look for who uses it also in a repository
    /// with many files.
    pub fn why_graph(&mut self, lane: LaneId, target: &str, uses: bool) -> Result<WhyGraph> {
        // A lane is asked about as its agent just left it. The main
        // directory is not looked at whole for a question: what is saved
        // there is recorded as it is saved.
        if !lane.is_main() {
            self.scan_lane(lane)?;
        }
        self.why_index()?;
        match parse_target(target)? {
            Asked::Symbol(file, name) => {
                let ids = self.work(lane)?.symbols.find(&file, &name);
                match ids.as_slice() {
                    [id] => self.why_symbol(lane, *id, None, uses),
                    [] => {
                        // What is not the name of a function may be a file whose name has a colon in it.
                        if self.work(lane)?.tree.contains(Path::new(target.trim())) {
                            return self.why_file(lane, Path::new(target.trim()), None);
                        }
                        let table = &self.work(lane)?.symbols;
                        let mut known: Vec<String> =
                            table.in_file(&file).filter_map(|(id, _)| table.qualified_name(id)).collect();
                        known.sort();
                        known.dedup();
                        Err(Error::Other(match known.is_empty() {
                            true => format!("{}: no functions are known in this file", file.display()),
                            false => format!(
                                "{}: nothing called `{name}`; the file has: {}",
                                file.display(),
                                known.join(", ")
                            ),
                        }))
                    }
                    several => Err(Error::Other(format!(
                        "{} things are called `{name}` in {}: ask about one by its lines ({}:<line>)",
                        several.len(),
                        file.display(),
                        file.display()
                    ))),
                }
            }
            Asked::File(file) => self.why_file(lane, &file, None),
            Asked::Lines(file, from, to) => {
                // The innermost function the first line is in; outside
                // every function, the lines are the file's.
                let spans = self.symbol_spans(lane, &file)?;
                match spans.iter().rfind(|(_, start, end)| *start <= from && from <= *end) {
                    Some((id, _, _)) => self.why_symbol(lane, *id, Some((from, to)), uses),
                    None => self.why_file(lane, &file, Some((from, to))),
                }
            }
        }
    }

    /// The content of a file as a lane has it.
    fn text_of(&self, lane: LaneId, file: &Path) -> Result<String> {
        let entry = self
            .work(lane)?
            .tree
            .get(file)
            .ok_or_else(|| Error::Other(format!("{}: no such file on {lane}", file.display())))?;
        Ok(String::from_utf8_lossy(&self.store.get_blob(&entry.blob)?.unwrap_or_default()).into_owned())
    }

    /// The symbols of a file as the lane has it, with the lines each is on.
    pub(crate) fn symbol_spans(&self, lane: LaneId, file: &Path) -> Result<Vec<(SymbolId, u32, u32)>> {
        let content = self.text_of(lane, file)?.into_bytes();
        let Some(dialect) = kito_parse::dialect_for_path(file) else { return Ok(Vec::new()) };
        let Some(parsed) = kito_parse::parse(dialect, &content) else { return Ok(Vec::new()) };
        let ids = kito_diff::ast::identify(&self.work(lane)?.symbols, file, &parsed);
        let line_of =
            |at: usize| 1 + content[..at.min(content.len())].iter().filter(|&&b| b == b'\n').count() as u32;
        Ok(parsed
            .symbols
            .iter()
            .zip(ids)
            .filter_map(|(symbol, id)| {
                let end = symbol.range.end.saturating_sub(1).max(symbol.range.start);
                Some((id?, line_of(symbol.range.start), line_of(end)))
            })
            .collect())
    }

    fn why_symbol(
        &mut self,
        lane: LaneId,
        id: SymbolId,
        asked: Option<(u32, u32)>,
        uses: bool,
    ) -> Result<WhyGraph> {
        let work = self.work(lane)?;
        let entry =
            work.symbols.get(id).cloned().ok_or_else(|| Error::Other(format!("no symbol {id} on {lane}")))?;
        let name = work.symbols.qualified_name(id).unwrap_or_else(|| entry.name.clone());
        let files = work.tree.len();
        let span = self.symbol_spans(lane, &entry.file)?.into_iter().find(|(found, _, _)| *found == id);

        // What happened to the function, and what its lines say: a line
        // may have been written for a reason that left no trace on the
        // function as such (a comment, or code older than Kito's reading
        // of the file).
        let reach = crate::semantic::Reach::of(&self.store, lane)?;
        let groups = self.symbol_groups(&reach, id)?;
        let mut notes = Vec::new();
        let (reasons, lines) = match span {
            Some((_, start, end)) => {
                let content = self.text_of(lane, &entry.file)?;
                let places = self.file_lines(lane, &reach, &entry.file, &content, true)?;
                let mine: Vec<Option<Place>> =
                    places.into_iter().skip(start as usize - 1).take((end - start + 1) as usize).collect();
                let more = self.file_groups(&reach, &entry.file)?;
                self.answer(
                    lane,
                    groups,
                    more,
                    Written { lines: &mine, first: start, asked },
                    About::Symbol(id),
                )
            }
            None => self.answer(
                lane,
                groups,
                Vec::new(),
                Written { lines: &[], first: 1, asked },
                About::Symbol(id),
            ),
        };
        if lines.iter().any(|run| run.reason.is_none()) {
            notes.push("some lines are older than what the history tells of this file".to_owned());
        }

        // Who uses it and what it uses, as far as the syntax tells.
        let (mut used_by, mut uses_list) = (Vec::new(), Vec::new());
        if uses || files <= USES_LOOKED_FOR_UP_TO {
            let (users, used) = self.neighbours(lane, id)?;
            for (from, line) in users.into_iter().take(LISTED) {
                used_by.push(Neighbour {
                    symbol: self.symbol_label(lane, from),
                    line: Some(line),
                    last: self.brief(lane, &reach, from),
                });
            }
            for to in used.into_iter().take(LISTED) {
                uses_list.push(Neighbour {
                    symbol: self.symbol_label(lane, to),
                    line: None,
                    last: self.brief(lane, &reach, to),
                });
            }
        } else {
            notes.push(format!(
                "who uses it was not looked for: the repository has {files} files (ask with `uses` to look anyway)"
            ));
        }

        let target = format!("{}:{name}", entry.file.display());
        Ok(WhyGraph {
            target: Target {
                kind: crate::semantic::kind_word(entry.kind).to_owned(),
                file: entry.file.display().to_string(),
                name: Some(name),
                id: Some(id.to_string()),
                lines: span.map(|(_, start, end)| (start, end)),
            },
            reasons,
            lines,
            used_by,
            uses: uses_list,
            contains: Vec::new(),
            in_progress: self.in_progress(lane, &target),
            decisions: self.decisions(|conflict| conflict.symbol == Some(id)),
            notes,
        })
    }

    /// Who uses a symbol (with the line of the use) and what it uses.
    fn neighbours(&mut self, lane: LaneId, id: SymbolId) -> Result<Neighbours> {
        let work = self.work(lane)?;
        let Some(file) = work.symbols.get(id).map(|entry| entry.file.clone()) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let frozen = self.frozen(&work.tree, &work.symbols)?;
        let snapshot = frozen.snapshot();
        let semantic = self.semantic();
        let inside = |mut from: Option<SymbolId>| {
            for _ in 0..64 {
                match from {
                    Some(found) if found == id => return true,
                    Some(found) => from = snapshot.symbols.get(found).and_then(|entry| entry.parent),
                    None => return false,
                }
            }
            false
        };
        let mut users: BTreeMap<SymbolId, u32> = BTreeMap::new();
        for usage in semantic.heuristic.usages_of(&snapshot, id).unwrap_or_default() {
            if let Some(reference) = kito_semantic::reference(&snapshot, id, &usage) {
                if !inside(Some(reference.from)) {
                    users.entry(reference.from).or_insert(reference.site.line);
                }
            }
        }
        let mut used: BTreeSet<SymbolId> = BTreeSet::new();
        for reference in semantic.heuristic.references_in(&snapshot, &file) {
            if inside(Some(reference.from)) && !inside(Some(reference.to)) {
                used.insert(reference.to);
            }
        }
        Ok((users.into_iter().collect(), used.into_iter().collect()))
    }

    /// A file: every reason it changed for, what it contains, and its
    /// lines by reason. With `asked`, only those lines and the reasons
    /// they were written for.
    fn why_file(&mut self, lane: LaneId, file: &Path, asked: Option<(u32, u32)>) -> Result<WhyGraph> {
        let content = self.text_of(lane, file)?;
        let reach = crate::semantic::Reach::of(&self.store, lane)?;
        let places = self.file_lines(lane, &reach, file, &content, true)?;
        let groups = self.file_groups(&reach, file)?;
        let (reasons, lines) = match asked {
            // Of some lines, the reasons that wrote them.
            Some(_) => self.answer(
                lane,
                Vec::new(),
                groups,
                Written { lines: &places, first: 1, asked },
                About::File(file),
            ),
            None => self.answer(
                lane,
                groups,
                Vec::new(),
                Written { lines: &places, first: 1, asked: None },
                About::File(file),
            ),
        };
        let mut notes = Vec::new();
        if lines.iter().any(|run| run.reason.is_none()) {
            notes.push("some lines are older than what the history tells of this file".to_owned());
        }
        // What it contains, each with the last reason it changed for.
        let mut contains = Vec::new();
        for (id, line, end) in self.symbol_spans(lane, file)?.into_iter().take(4 * LISTED) {
            if asked.is_none_or(|(from, to)| line <= to && from <= end) {
                contains.push(Neighbour {
                    symbol: self.symbol_label(lane, id),
                    line: Some(line),
                    last: self.brief(lane, &reach, id),
                });
            }
        }
        let target = file.display().to_string();
        Ok(WhyGraph {
            target: Target {
                kind: "file".to_owned(),
                file: target.clone(),
                name: None,
                id: None,
                lines: asked,
            },
            reasons,
            lines,
            used_by: Vec::new(),
            uses: Vec::new(),
            contains,
            in_progress: self.in_progress(lane, &target),
            decisions: self
                .decisions(|conflict| conflict.symbol.is_none() && conflict.file.as_deref() == Some(file)),
            notes,
        })
    }

    /// The other lanes that are on a target now: for a function, the ones
    /// on the function itself or on code that uses it, not every lane that
    /// is somewhere in its file.
    fn in_progress(&self, asking: LaneId, target: &str) -> Vec<InProgress> {
        let function = target.contains(':');
        let mut lanes: Vec<InProgress> = Vec::new();
        for overlap in self.who_touches(target).unwrap_or_default() {
            if overlap.lane == asking.to_string() || lanes.iter().any(|known| known.lane == overlap.lane) {
                continue;
            }
            if function && !matches!(overlap.kind.as_str(), "function" | "caller") {
                continue;
            }
            let how = match overlap.kind.as_str() {
                "caller" => "works on code that uses it".to_owned(),
                _ => overlap.detail.clone(),
            };
            lanes.push(InProgress {
                lane: overlap.lane,
                agent: overlap.agent,
                person: overlap.owner,
                intent: overlap.intent,
                how,
            });
        }
        lanes
    }

    fn decisions(&self, about: impl Fn(&kito_core::Conflict) -> bool) -> Vec<Decision> {
        self.conflicts(true)
            .into_iter()
            .filter(|info| info.id.parse().ok().and_then(|id| self.store.conflict(id)).is_some_and(&about))
            .map(|info| Decision {
                conflict: info.id,
                kind: info.kind,
                lane: Some(info.lane),
                decided: info.resolution,
                applied: info.resolved,
            })
            .collect()
    }

    /// Of the reasons a file changed for, the ones that wrote `wanted`,
    /// newest first.
    fn reasons_at(
        &self,
        lane: LaneId,
        reach: &crate::semantic::Reach,
        file: &Path,
        wanted: &BTreeSet<Place>,
    ) -> Result<Vec<Reason>> {
        let mut groups: Vec<Group> = self
            .file_groups(reach, file)?
            .into_iter()
            .filter(|group| wanted.contains(&group.place))
            .collect();
        for group in &mut groups {
            group.hows = vec!["wrote these lines".to_owned()];
        }
        self.in_order(&mut groups);
        Ok(groups.iter().rev().map(|group| self.reason_of(lane, group, About::File(file))).collect())
    }

    /// The reasons some lines of a file were written for, as `lane` has
    /// the file: newest first. `lines`: `None` for the whole file. A file
    /// the lane does not have has none.
    pub(crate) fn reasons_of_lines(
        &mut self,
        lane: LaneId,
        file: &Path,
        lines: Option<&[(u32, u32)]>,
    ) -> Result<Vec<Reason>> {
        self.why_index()?;
        let Ok(content) = self.text_of(lane, file) else { return Ok(Vec::new()) };
        let reach = crate::semantic::Reach::of(&self.store, lane)?;
        let places = self.file_lines(lane, &reach, file, &content, true)?;
        let asked =
            |line: u32| lines.is_none_or(|lines| lines.iter().any(|(from, to)| *from <= line && line <= *to));
        let wanted: BTreeSet<Place> = places
            .iter()
            .enumerate()
            .filter(|(at, _)| asked(*at as u32 + 1))
            .filter_map(|(_, place)| *place)
            .collect();
        self.reasons_at(lane, &reach, file, &wanted)
    }

    /// The reasons behind what a lane replaced or removed: for each file
    /// it changed, the lines it found there and no longer has. Each with
    /// its file, newest first within a file.
    pub(crate) fn reasons_replaced(&mut self, lane: LaneId) -> Result<Vec<(String, Reason)>> {
        self.why_index()?;
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let changed: Vec<(PathBuf, kito_core::BlobId, Option<kito_core::BlobId>)> = at
            .base_tree
            .differences(&at.work.tree)
            .filter_map(|path| {
                let before = at.base_tree.get(path)?.blob;
                Some((path.to_owned(), before, at.work.tree.get(path).map(|entry| entry.blob)))
            })
            .collect();
        let reach = crate::semantic::Reach::of(&self.store, lane)?;
        let mut found = Vec::new();
        for (file, before, after) in changed {
            let text = |blob: &kito_core::BlobId| -> Result<String> {
                Ok(String::from_utf8_lossy(&self.store.get_blob(blob)?.unwrap_or_default()).into_owned())
            };
            let was = text(&before)?;
            let is = match &after {
                Some(blob) => text(blob)?,
                None => String::new(),
            };
            let places = self.file_lines(lane, &reach, &file, &was, false)?;
            let (old, new): (Vec<&str>, Vec<&str>) = (was.lines().collect(), is.lines().collect());
            let mut wanted: BTreeSet<Place> = BTreeSet::new();
            for op in similar::capture_diff_slices(similar::Algorithm::Myers, &old, &new) {
                let (tag, gone, _) = op.as_tag_tuple();
                if matches!(tag, similar::DiffTag::Delete | similar::DiffTag::Replace) {
                    wanted.extend(gone.filter_map(|line| places.get(line).copied().flatten()));
                }
            }
            for reason in self.reasons_at(lane, &reach, &file, &wanted)? {
                found.push((file.display().to_string(), reason));
            }
        }
        Ok(found)
    }

    /// The reasons whose words (or task, or people) contain every word of
    /// `about`, newest first, each with what it changed.
    pub fn reasons(&mut self, about: &str, limit: usize) -> Result<Vec<ReasonHit>> {
        self.why_index()?;
        let Some(index) = self.why_index.as_ref() else { return Ok(Vec::new()) };
        let words: Vec<String> = about.split_whitespace().map(str::to_lowercase).collect();
        let mut hits: Vec<(Hlc, ReasonHit)> = Vec::new();
        for (key, touched) in &index.touched {
            let (why, task) = self.reason_words(key);
            // What Kito says of its own doing is nobody's reason.
            if why == ANALYSIS || (key.lane.is_main() && integrated_lane(&why).is_some()) {
                continue;
            }
            if key.intent.is_none() && key.checkpoint.is_none() {
                continue;
            }
            let record = self.store.lane_record(key.lane);
            let person = record.and_then(|lane| lane.owner.as_ref()).map(|owner| owner.name.clone());
            let by: Vec<String> = touched.authors.iter().cloned().collect();
            let text = format!(
                "{why} {} {} {}",
                task.clone().unwrap_or_default(),
                by.join(" "),
                person.clone().unwrap_or_default()
            )
            .to_lowercase();
            if !words.iter().all(|word| text.contains(word)) {
                continue;
            }
            let (changed, changed_more) = self.touched_names(LaneId::MAIN, touched, About::Nothing);
            let checkpoint = match key.lane.is_main() {
                true => index.checkpoint_at(touched.last),
                false => index.integration_of(key.lane, touched.last),
            };
            hits.push((
                touched.last,
                ReasonHit {
                    why,
                    task,
                    by,
                    person,
                    lane: (!key.lane.is_main()).then(|| key.lane.to_string()),
                    checkpoint: checkpoint.map(|ck| ck.id.to_string()),
                    at_ms: touched.last.wall_ms,
                    changed,
                    changed_more,
                },
            ));
        }
        hits.sort_by_key(|hit| std::cmp::Reverse(hit.0));
        Ok(hits.into_iter().take(limit.max(1)).map(|(_, hit)| hit).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_a_function_a_file_or_lines() {
        assert!(
            matches!(parse_target("src/shop.ts:total").unwrap(), Asked::Symbol(file, name) if file == Path::new("src/shop.ts") && name == "total")
        );
        assert!(
            matches!(parse_target("src/shop.ts").unwrap(), Asked::File(file) if file == Path::new("src/shop.ts"))
        );
        assert!(matches!(parse_target("src/shop.ts:12").unwrap(), Asked::Lines(_, 12, 12)));
        assert!(matches!(parse_target("src/shop.ts:20-12").unwrap(), Asked::Lines(_, 12, 20)));
        assert!(
            matches!(parse_target("src/shop.ts:Cart.total").unwrap(), Asked::Symbol(_, name) if name == "Cart.total")
        );
        assert!(parse_target("  ").is_err());
    }

    #[test]
    fn each_line_belongs_to_the_version_that_wrote_it() {
        let versions: Vec<String> =
            ["a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\nd\n", "B\nc\nd\n"].iter().map(|v| (*v).to_owned()).collect();
        assert_eq!(blame(&versions), [1, 0, 2]);
        assert_eq!(blame(&versions[..1]), [0, 0, 0]);
        assert!(blame(&[]).is_empty());
    }

    #[test]
    fn an_integration_names_its_lane() {
        assert_eq!(integrated_lane("integrate lane/3: prices in cents"), Some(LaneId(3)));
        assert_eq!(integrated_lane("integrate lane/a (builder)"), Some(LaneId(10)));
        assert_eq!(integrated_lane("prices in cents"), None);
    }
}
