//! Integration: a lane's work becomes part of the main lane.
//!
//! It happens in three steps, so that the verification (which can take
//! minutes) never holds the repository:
//!
//! 1. [`Repo::prepare_integration`] merges what the lane did since its base
//!    into the current state of the main lane. Conflicts stop it here.
//! 2. The merged result is materialized in a directory of its own
//!    ([`Repo::verification_copy`]) and the configured commands run on it.
//! 3. [`Repo::finish_integration`] writes the result into the main working
//!    directory, records it as ops of the main lane signed by the lane's
//!    agent, creates a checkpoint, and brings the lane up to it.
//!
//! The ops recorded on the main lane are new ops: the first one has two
//! parents, the head of the main lane and the head of the lane, and the
//! symbols keep the IDs they had in the lane.

use crate::error::{Error, Result};
use crate::refs::{RefAnswers, Side};
use crate::repo::{Repo, Unsettled};
use kito_core::{
    Author, BlobId, Checkpoint, Conflict, ConflictId, ConflictKind, Intent, LaneId, LaneState, OpId,
    Resolution, SymbolId, SymbolTable, Tree, TreeEntry, Verification,
};
use kito_integrate::{
    AutoResolution, Confinement, ConflictInfo, IntegrationResult, Outcome, VerificationReport,
};
use kito_merge::{Choice, Found, Subject};
use kito_store::STORE_DIR;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The checks of an integration, ready to run on a copy of the merged result.
pub struct Checks {
    pub dir: PathBuf,
    pub commands: Vec<String>,
    pub timeout: Duration,
    /// How they are confined.
    pub confinement: Confinement,
}

impl Checks {
    pub async fn run(&self) -> VerificationReport {
        kito_integrate::verify(&self.dir, &self.commands, self.timeout, &self.confinement).await
    }
}

/// A merge that composed, waiting to be verified and recorded.
pub struct Prepared {
    pub lane: LaneId,
    /// What the merge was computed on: if either moved, it is computed again.
    main_head: Option<OpId>,
    lane_ops: u64,
    outcome: kito_merge::Outcome,
    /// Conflicts this merge settled with the decisions taken on them.
    settled: Vec<ConflictId>,
    /// Renames that reached the other side's uses of the symbol.
    propagated: Vec<AutoResolution>,
    /// Language servers that failed while the uses were looked for.
    problems: Vec<String>,
}

/// What [`Repo::prepare_integration`] found.
pub enum Preparation {
    /// Nothing more to do: this is the result.
    Done(Box<IntegrationResult>),
    /// The merge composed: verify it, then finish.
    Ready(Box<Prepared>),
    /// One side renamed something the other still calls by its old name:
    /// ask the providers of level 2 who uses it ([`crate::refs::answer`]),
    /// then prepare again with their answers.
    Ask(Box<crate::refs::RefPlan>),
}

/// What a merge writes on the main lane, as entries of a tree.
type Written = std::collections::BTreeMap<PathBuf, Option<TreeEntry>>;

/// A lane that is checked together with others.
#[derive(Clone, Debug)]
pub struct Member {
    pub lane: LaneId,
    /// What its merge writes: what was checked, and what must be written
    /// when the lane is integrated.
    written: Written,
}

/// What to do with lanes that ask to be integrated at the same time.
#[derive(Default)]
pub struct BatchPlan {
    /// The ones that have their answer already.
    pub ended: Vec<(LaneId, IntegrationResult)>,
    /// The ones to check together, in the order they asked.
    pub members: Vec<Member>,
    /// Their merged results in one copy, ready for the checks. `None`
    /// when fewer than two can be checked together.
    pub checks: Option<Checks>,
    /// The ones that go by themselves.
    pub alone: Vec<LaneId>,
}

/// Where lanes are integrated together: the repository, which is held
/// only for the moments it is needed, and the checks, which are not run
/// while holding it.
pub trait Bench {
    fn repo<R>(&self, f: impl FnOnce(&mut Repo) -> Result<R>) -> Result<R>;
    /// Runs the checks on the copy they were prepared for.
    fn check(&self, checks: &Checks) -> VerificationReport;
    /// Integrates one lane by itself, with checks of its own.
    fn alone(&self, lane: LaneId) -> IntegrationResult;
    /// Who lets the lane in, when a person does.
    fn approver(&self, lane: LaneId) -> Option<Author>;
    /// A lane has its answer.
    fn done(&self, lane: LaneId, result: IntegrationResult);
}

/// Integrates lanes that asked at the same time. The ones that can be
/// are checked in one run of the commands, on the main lane with all of
/// them merged in; if the commands pass each is integrated, in the order
/// they asked. If they fail, the lanes are split in two and each half is
/// tried again, down to the lane that fails by itself.
pub fn integrate_batch<B: Bench>(bench: &B, lanes: &[LaneId]) {
    let mut groups: std::collections::VecDeque<Vec<LaneId>> = std::collections::VecDeque::new();
    groups.push_back(lanes.to_vec());
    while let Some(group) = groups.pop_front() {
        if let [lane] = group[..] {
            bench.done(lane, bench.alone(lane));
            continue;
        }
        let plan = match bench.repo(|repo| repo.batch_plan(&group)) {
            Ok(plan) => plan,
            // Whatever went wrong, each lane is still owed its own answer.
            Err(_) => {
                groups.extend(group.into_iter().map(|lane| vec![lane]));
                continue;
            }
        };
        for (lane, result) in plan.ended {
            bench.done(lane, result);
        }
        if let Some(checks) = &plan.checks {
            let mut report = bench.check(checks);
            let together: Vec<LaneId> = plan.members.iter().map(|member| member.lane).collect();
            match report.passed {
                true => {
                    report.batch = together.iter().map(LaneId::to_string).collect();
                    for member in &plan.members {
                        let approver = bench.approver(member.lane);
                        let landed = bench.repo(|repo| repo.land_member(member, report.clone(), approver));
                        match landed {
                            Ok(Some(result)) => bench.done(member.lane, result),
                            // Not what was checked any more: by itself.
                            _ => bench.done(member.lane, bench.alone(member.lane)),
                        }
                    }
                }
                // One of them fails the checks, or they fail together:
                // the first half, then the second.
                false => {
                    let (first, second) = together.split_at(together.len() / 2);
                    groups.push_front(second.to_vec());
                    groups.push_front(first.to_vec());
                }
            }
        }
        groups.extend(plan.alone.into_iter().map(|lane| vec![lane]));
    }
}

/// A decision on a conflict, as a client gives it./// A decision on a conflict, as a client gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Main,
    Lane,
    Text(String),
    /// For a `semantic_ref` conflict: the uses are of the renamed symbol,
    /// and take its new name.
    Rewrite,
    /// For a `semantic_ref` conflict: they are something else, and stay.
    Keep,
}

/// The text that, in a `semantic_ref` conflict, is on the side that renamed.
const RENAMED: &[u8] = b"renamed: ";

fn kind_word(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BodyBody => "body_body",
        ConflictKind::RemoveModify => "remove_modify",
        ConflictKind::RenameRename => "rename_rename",
        ConflictKind::SemanticRef => "semantic_ref",
        ConflictKind::Text => "text",
    }
}

fn subject_of(conflict: &Conflict) -> Subject {
    match (conflict.symbol, &conflict.file) {
        (Some(id), _) => Subject::Symbol(id),
        (None, Some(file)) => Subject::File(file.clone()),
        (None, None) => Subject::File(PathBuf::new()),
    }
}

/// Removes a file of a working directory, and the directories it leaves empty.
pub(crate) fn remove_in(root: &Path, path: &Path) -> Result<()> {
    let abs = root.join(path);
    match fs::remove_file(&abs) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(&abs)(e)),
    }
    let mut dir = path.parent();
    while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
        if fs::remove_dir(root.join(d)).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

impl Repo {
    fn conflict_info(&self, conflict: &Conflict) -> ConflictInfo {
        let text = |blob: &Option<BlobId>| {
            blob.and_then(|id| self.store.get_blob(&id).ok().flatten())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };
        ConflictInfo {
            id: conflict.id.to_string(),
            kind: kind_word(conflict.kind).to_owned(),
            lane: conflict.assigned_to.map(|lane| lane.to_string()).unwrap_or_default(),
            file: conflict.file.as_ref().map(|f| f.display().to_string()),
            symbol: conflict.name.clone(),
            symbol_id: conflict.symbol.map(|id| id.to_string()),
            base: text(&conflict.base_text),
            main: text(&conflict.main_text),
            lane_text: text(&conflict.lane_text),
            resolution: conflict.resolution.map(|r| {
                match r {
                    Resolution::Main => "main",
                    Resolution::Lane => "lane",
                    Resolution::Text(_) => "text",
                }
                .to_owned()
            }),
            resolved: conflict.resolved_by.is_some(),
        }
    }

    /// The conflicts waiting for a decision or for the integration that
    /// applies it; with `all`, the settled ones too.
    /// The lane that has a conflict to decide.
    pub fn conflict_lane(&self, id: ConflictId) -> Option<LaneId> {
        self.store.conflict(id).and_then(|conflict| conflict.assigned_to)
    }

    pub fn conflicts(&self, all: bool) -> Vec<ConflictInfo> {
        self.store
            .conflicts()
            .filter(|c| all || c.resolved_by.is_none())
            .map(|c| self.conflict_info(c))
            .collect()
    }

    fn open_conflicts(&self, lane: LaneId) -> Vec<Conflict> {
        self.store
            .conflicts()
            .filter(|c| c.assigned_to == Some(lane) && c.resolved_by.is_none())
            .cloned()
            .collect()
    }

    /// Records the decision on a conflict. It is applied by the next
    /// integration of the lane, if by then the two sides still are what the
    /// decision was taken on.
    pub fn resolve_conflict(&mut self, id: ConflictId, decision: Decision) -> Result<ConflictInfo> {
        let mut conflict = self.store.conflict(id).cloned().ok_or_else(|| {
            Error::Other(format!("{id}: this conflict does not exist (see `kito conflicts`)"))
        })?;
        if conflict.resolved_by.is_some() {
            return Err(Error::Other(format!("{id} is already settled")));
        }
        let lane = conflict.assigned_to.filter(|lane| self.lanes.contains_key(lane));
        let lane = lane.ok_or_else(|| Error::Other(format!("{id}: its lane is closed")))?;
        let about_uses = conflict.kind == ConflictKind::SemanticRef;
        // In a conflict about uses, the side that renamed is the one whose
        // text says so; following it is rewriting the uses.
        let main_renamed = conflict
            .main_text
            .and_then(|blob| self.store.get_blob(&blob).ok().flatten())
            .is_some_and(|text| text.starts_with(RENAMED));
        conflict.resolution = Some(match decision {
            Decision::Main => Resolution::Main,
            Decision::Lane => Resolution::Lane,
            Decision::Rewrite | Decision::Keep if !about_uses => {
                return Err(Error::Other(format!(
                    "{id} is not about the uses of a renamed symbol: choose `main`, `lane` or a text"
                )));
            }
            Decision::Rewrite if main_renamed => Resolution::Main,
            Decision::Rewrite => Resolution::Lane,
            Decision::Keep if main_renamed => Resolution::Lane,
            Decision::Keep => Resolution::Main,
            Decision::Text(_) if about_uses => {
                return Err(Error::Other(format!(
                    "{id} is about the uses of a renamed symbol: choose `rewrite` (they take the new name) or `keep` (they stay as they are)"
                )));
            }
            Decision::Text(text) => {
                if text.trim().is_empty() {
                    return Err(Error::Other(
                        "the new text is empty: to drop the lane's version choose `main`".into(),
                    ));
                }
                Resolution::Text(self.store.put_blob(text.as_bytes())?)
            }
        });
        self.store.put_conflict(&conflict)?;
        // Every conflict of the lane has a decision: it can ask again.
        if self.open_conflicts(lane).iter().all(|c| c.resolution.is_some()) {
            self.put_lane_state(lane, LaneState::Ready)?;
        }
        Ok(self.conflict_info(&conflict))
    }

    /// Merges the lane into the main lane as they are now. With `answers`,
    /// the uses each side has of what the other renamed are rewritten
    /// first; `uses` says what was decided about the doubtful ones.
    fn merge_lane(
        &mut self,
        lane: LaneId,
        decisions: &HashMap<Subject, Choice>,
        answers: Option<&RefAnswers>,
        uses: &HashMap<SymbolId, bool>,
    ) -> Result<(kito_merge::Outcome, Vec<AutoResolution>)> {
        let spread = match answers {
            Some(answers) => Some(self.propagate(lane, answers, uses)?),
            None => None,
        };
        let main_tree =
            spread.as_ref().and_then(|s| s.main.clone()).unwrap_or_else(|| self.main.tree.clone());
        // Files parsed for the merge stay parsed for the capture that
        // records its result, and those the captures parsed are not parsed
        // again here.
        let cache = std::cell::RefCell::new(std::mem::take(&mut self.parse_cache));
        let outcome = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane)).map(|at| {
            let store = &self.store;
            let parse = |path: &Path, blob: &BlobId, _: &[u8]| {
                crate::semantic::load(store, Some(&mut cache.borrow_mut()), path, blob, None)
                    .ok()
                    .flatten()
                    .map(|parsed| parsed.file.clone())
            };
            let lane_tree = spread.as_ref().and_then(|s| s.lane.as_ref()).unwrap_or(&at.work.tree);
            kito_merge::merge(&kito_merge::Input {
                base: kito_merge::Side {
                    tree: spread.as_ref().and_then(|s| s.base.as_ref()).unwrap_or(&at.base_tree),
                    symbols: &at.base_symbols,
                },
                main: kito_merge::Side { tree: &main_tree, symbols: &self.main.symbols },
                lane: kito_merge::Side { tree: lane_tree, symbols: &at.work.symbols },
                blob: &|id| store.get_blob(id).ok().flatten(),
                resolutions: decisions,
                parse: Some(&parse),
            })
        });
        self.parse_cache = cache.into_inner();
        let mut outcome = outcome?;
        let mut done = Vec::new();
        if let Some(spread) = spread {
            // The uses rewritten on the main lane's side are part of the
            // result also where the merge had nothing else to do.
            for path in main_tree.differences(&self.main.tree) {
                if let (false, Some(entry)) = (outcome.files.contains_key(path), main_tree.get(path)) {
                    let content = self.store.get_blob(&entry.blob)?.unwrap_or_default();
                    outcome
                        .files
                        .insert(path.to_owned(), Some(kito_merge::FileOut { content, mode: entry.mode }));
                }
            }
            outcome.conflicts.extend(spread.doubts);
            done = spread.done;
        }
        Ok((outcome, done))
    }

    /// Captures what was saved on the main lane in the files a lane's work
    /// touches: nothing unrecorded may be merged over, or written over.
    /// Returns `true` if something was recorded.
    pub(crate) fn capture_main_under(&mut self, lane: LaneId, also: &[PathBuf]) -> Result<bool> {
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let mut paths: Vec<PathBuf> = at.work.tree.differences(&at.base_tree).map(Path::to_owned).collect();
        paths.extend(also.iter().cloned());
        Ok(!self.capture_lane(LaneId::MAIN, paths)?.is_empty())
    }

    /// Step 1: captures what is still unsaved on both sides and merges the
    /// lane into the main lane.
    ///
    /// `answers`: what the providers of level 2 answered to the questions
    /// of a [`Preparation::Ask`]. It is given none the first time; it asks
    /// only if something was renamed, and again if the lanes moved since.
    pub fn prepare_integration(&mut self, lane: LaneId, answers: Option<&RefAnswers>) -> Result<Preparation> {
        if lane.is_main() {
            return Err(Error::Other("the main lane is what lanes are integrated into".into()));
        }
        // The lane is scanned whole: whatever its agent saved is part of
        // what it asks to integrate. Of the main directory only the files
        // involved are looked at: the rest is the watcher's business.
        self.scan_lane(lane)?;
        self.capture_main_under(lane, &[])?;
        // A lane goes in for what it said it would change, and nothing else.
        let outside = self.out_of_scope(lane)?;
        if !outside.is_empty() {
            let mut result = IntegrationResult::new(lane.to_string(), Outcome::OutOfScope);
            result.message = Some(match self.lane_scope(lane)?.is_empty() {
                true => "the lane has no scope: declare what it changes, then ask again".to_owned(),
                false => {
                    "the lane changed things outside the scope it declared: extend the scope to them, or \
                          undo them, then ask again"
                        .to_owned()
                }
            });
            result.out_of_scope = outside;
            return Ok(Preparation::Done(Box::new(result)));
        }
        let answers = answers.filter(|answers| self.answers_hold(lane, answers));
        if answers.is_none() {
            if let Some(plan) = self.reference_plan(lane)? {
                return Ok(Preparation::Ask(Box::new(plan)));
            }
        }
        let open = self.open_conflicts(lane);
        let blob_of = |text: &Option<Vec<u8>>| text.as_ref().map(|t| BlobId::of(t));
        let same_texts = |conflict: &Conflict, found: &Found| {
            conflict.base_text == blob_of(&found.base)
                && conflict.main_text == blob_of(&found.main)
                && conflict.lane_text == blob_of(&found.lane)
        };

        // What conflicts as things are now, and which of those conflicts
        // already have a decision taken on these very texts.
        let no_uses = HashMap::new();
        let (mut found, mut propagated) = self.merge_lane(lane, &HashMap::new(), answers, &no_uses)?;
        // The merge may write files of the main lane the lane did not touch
        // (a symbol moved there): if something unrecorded was in them, the
        // merge is made again on what is there now.
        for _ in 0..3 {
            let written: Vec<PathBuf> = found.files.keys().cloned().collect();
            if !self.capture_main_under(lane, &written)? {
                break;
            }
            // The main lane moved: what was asked about it is no longer so.
            if let Some(plan) = self.reference_plan(lane)? {
                return Ok(Preparation::Ask(Box::new(plan)));
            }
            (found, propagated) = self.merge_lane(lane, &HashMap::new(), None, &no_uses)?;
        }
        let mut decisions: HashMap<Subject, Choice> = HashMap::new();
        let mut uses: HashMap<SymbolId, bool> = HashMap::new();
        let mut settled = Vec::new();
        for conflict in &found.conflicts {
            let decided = open.iter().find(|c| {
                subject_of(c) == conflict.subject && c.kind == conflict.kind && same_texts(c, conflict)
            });
            if let Some((known, resolution)) = decided.and_then(|c| c.resolution.map(|r| (c, r))) {
                if let (ConflictKind::SemanticRef, Subject::Symbol(symbol)) =
                    (conflict.kind, &conflict.subject)
                {
                    // Taking the side that renamed is rewriting the uses.
                    let renamer = answers.and_then(|answers| answers.renamed_by(*symbol));
                    let rewrite = matches!(
                        (resolution, renamer),
                        (Resolution::Main, Some(Side::Main)) | (Resolution::Lane, Some(Side::Lane))
                    );
                    uses.insert(*symbol, rewrite);
                } else {
                    let choice = match resolution {
                        Resolution::Main => Choice::Main,
                        Resolution::Lane => Choice::Lane,
                        Resolution::Text(blob) => {
                            Choice::Text(self.store.get_blob(&blob)?.unwrap_or_default())
                        }
                    };
                    decisions.insert(conflict.subject.clone(), choice);
                }
                settled.push(known.id);
            }
        }
        let all_decided = !found.conflicts.is_empty() && settled.len() == found.conflicts.len();
        let (outcome, propagated) = match all_decided {
            true => self.merge_lane(lane, &decisions, answers, &uses)?,
            false => (found, propagated),
        };

        if !outcome.conflicts.is_empty() {
            let infos = self.record_conflicts(lane, &outcome.conflicts, &open, &same_texts)?;
            let mut result = IntegrationResult::new(lane.to_string(), Outcome::Conflicts);
            let mut message = format!(
                "{} to decide with kito.resolve_conflict (or `kito resolve`), then ask again",
                match infos.iter().filter(|c| c.resolution.is_none()).count() {
                    1 => "1 conflict".to_owned(),
                    n => format!("{n} conflicts"),
                }
            );
            // Why the uses of a renamed symbol are asked about and not known.
            for problem in answers.iter().flat_map(|answers| &answers.problems) {
                message.push_str(&format!("; {problem}"));
            }
            result.message = Some(message);
            result.conflicts = infos;
            return Ok(Preparation::Done(Box::new(result)));
        }
        // Conflicts recorded before that the lane's later work made moot.
        for conflict in open.iter().filter(|c| !settled.contains(&c.id)) {
            let mut moot = conflict.clone();
            moot.resolved_by = self.lanes[&lane].work.head;
            self.store.put_conflict(&moot)?;
        }
        if outcome.files.is_empty() {
            // The decisions taken left the main lane as it is: they are
            // applied by doing nothing.
            for id in &settled {
                if let Some(mut conflict) = self.store.conflict(*id).cloned() {
                    conflict.resolved_by = self.main.head;
                    self.store.put_conflict(&conflict)?;
                }
            }
            // Nothing the main lane does not have: the lane just catches up.
            let base = self.lanes[&lane].record.base;
            let base = self.latest_checkpoint_at_head().unwrap_or(base);
            self.fast_forward(lane, base)?;
            let mut result = IntegrationResult::new(lane.to_string(), Outcome::NothingToIntegrate);
            result.your_next_base = Some(base.to_string());
            result.message = Some("the main lane already has everything the lane has".to_owned());
            return Ok(Preparation::Done(Box::new(result)));
        }
        let at = &self.lanes[&lane];
        Ok(Preparation::Ready(Box::new(Prepared {
            lane,
            main_head: self.main.head,
            lane_ops: at.work.ops,
            outcome,
            settled,
            propagated,
            problems: answers.map(|answers| answers.problems.clone()).unwrap_or_default(),
        })))
    }

    fn latest_checkpoint_at_head(&self) -> Option<kito_core::CheckpointId> {
        self.store.checkpoints().next_back().filter(|c| Some(c.head) == self.main.head).map(|c| c.id)
    }

    /// Brings the conflict records of a lane in line with what a merge
    /// found: a conflict that is still there keeps its number (and its
    /// decision, if the texts are the same), a new one gets a record, one
    /// that is gone is closed.
    fn record_conflicts(
        &mut self,
        lane: LaneId,
        found: &[Found],
        open: &[Conflict],
        same_texts: &dyn Fn(&Conflict, &Found) -> bool,
    ) -> Result<Vec<ConflictInfo>> {
        let (main_head, lane_head) = (self.main.head, self.lanes[&lane].work.head);
        let sides: Vec<OpId> = main_head.into_iter().chain(lane_head).collect();
        let mut current = Vec::new();
        for conflict in found {
            let known = open.iter().find(|c| subject_of(c) == conflict.subject && c.kind == conflict.kind);
            let mut put = |text: &Option<Vec<u8>>| -> Result<Option<BlobId>> {
                match text {
                    Some(text) => Ok(Some(self.store.put_blob(text)?)),
                    None => Ok(None),
                }
            };
            let texts = (put(&conflict.base)?, put(&conflict.main)?, put(&conflict.lane)?);
            let record = Conflict {
                id: known.map_or_else(|| self.store.next_conflict_id(), |c| c.id),
                symbol: match &conflict.subject {
                    Subject::Symbol(id) => Some(*id),
                    Subject::File(_) => None,
                },
                sides: sides.clone(),
                kind: conflict.kind,
                assigned_to: Some(lane),
                resolved_by: None,
                file: Some(conflict.file.clone()),
                name: conflict.name.clone(),
                base_text: texts.0,
                main_text: texts.1,
                lane_text: texts.2,
                // A decision holds for the texts it was taken on.
                resolution: known.filter(|c| same_texts(c, conflict)).and_then(|c| c.resolution),
            };
            if known != Some(&record) {
                self.store.put_conflict(&record)?;
            }
            current.push(record);
        }
        for gone in open.iter().filter(|c| !current.iter().any(|now| now.id == c.id)) {
            let mut closed = gone.clone();
            closed.resolved_by = lane_head;
            self.store.put_conflict(&closed)?;
        }
        let state = match current.iter().find(|c| c.resolution.is_none()) {
            Some(first) => LaneState::Blocked(first.id),
            None => LaneState::Ready,
        };
        self.put_lane_state(lane, state)?;
        Ok(current.iter().map(|c| self.conflict_info(c)).collect())
    }

    /// The tree a prepared merge leads to.
    fn merged_tree(&mut self, prepared: &Prepared) -> Result<Tree> {
        let mut entries: std::collections::BTreeMap<PathBuf, TreeEntry> =
            self.main.tree.iter().map(|(path, entry)| (path.to_owned(), *entry)).collect();
        for (path, file) in &prepared.outcome.files {
            match file {
                Some(file) => {
                    let blob = match self.main.tree.get(path) {
                        Some(before) => self.store.put_blob_like(&file.content, &before.blob)?,
                        None => self.store.put_blob(&file.content)?,
                    };
                    entries.insert(path.clone(), TreeEntry { blob, mode: file.mode });
                }
                None => {
                    entries.remove(path);
                }
            }
        }
        let mut tree = Tree::new();
        for (path, entry) in entries {
            tree.apply(&kito_core::OpKind::FileAdd { path, blob: entry.blob, mode: entry.mode })?;
        }
        Ok(tree)
    }

    /// What a prepared merge writes on the main lane: its files as
    /// entries of a tree; `None`: the file goes.
    fn written_by(&mut self, prepared: &Prepared) -> Result<Written> {
        let mut written = Written::new();
        for (path, file) in &prepared.outcome.files {
            let entry = match file {
                Some(file) => {
                    let blob = match self.main.tree.get(path) {
                        Some(before) => self.store.put_blob_like(&file.content, &before.blob)?,
                        None => self.store.put_blob(&file.content)?,
                    };
                    Some(TreeEntry { blob, mode: file.mode })
                }
                None => None,
            };
            written.insert(path.clone(), entry);
        }
        Ok(written)
    }

    /// `true` if what the lane's merge writes does not depend on what
    /// other lanes bring: it renamed and moved nothing, and no rename of
    /// the main lane reached its code. Such a lane can be checked together
    /// with others that write other files.
    fn stands_alone(&self, prepared: &Prepared) -> Result<bool> {
        if !prepared.propagated.is_empty() {
            return Ok(false);
        }
        let at = self.lanes.get(&prepared.lane).ok_or(Error::NoSuchLane(prepared.lane))?;
        const PAGE: usize = 1024;
        let mut start = at.record.since;
        loop {
            let ops = self.store.lane_ops(prepared.lane, start, PAGE)?;
            let moves = |kind: &kito_core::OpKind| {
                matches!(
                    kind,
                    kito_core::OpKind::Rename { .. }
                        | kito_core::OpKind::Move { .. }
                        | kito_core::OpKind::FileRename { .. }
                )
            };
            if ops.iter().any(|op| moves(&op.kind)) {
                return Ok(false);
            }
            if ops.len() < PAGE {
                return Ok(true);
            }
            start += ops.len() as u64;
        }
    }

    /// How many lanes may be checked in one run: 1 when the repository
    /// has no checks, since nothing is waited for then.
    pub fn batch_limit(&self) -> usize {
        match self.config.verify.commands.is_empty() {
            true => 1,
            false => self.config.verify.batch.max(1),
        }
    }

    /// Looks at lanes that ask to be integrated at the same time. Some
    /// have their answer at once (nothing to integrate, a conflict, out
    /// of scope). Of the others, the ones whose merges write different
    /// files and stand alone are to be checked together: their merged
    /// results are put in one copy, ready for the checks. The rest go one
    /// by one.
    pub fn batch_plan(&mut self, lanes: &[LaneId]) -> Result<BatchPlan> {
        let mut plan = BatchPlan::default();
        let mut taken: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        for lane in lanes {
            let prepared = match self.prepare_integration(*lane, None) {
                Ok(Preparation::Ready(prepared)) => prepared,
                Ok(Preparation::Done(result)) => {
                    plan.ended.push((*lane, *result));
                    continue;
                }
                // Something has to be asked first: by itself.
                Ok(Preparation::Ask(_)) => {
                    plan.alone.push(*lane);
                    continue;
                }
                Err(e) => {
                    plan.ended.push((*lane, IntegrationResult::failed(lane.to_string(), e.to_string())));
                    continue;
                }
            };
            let written = self.written_by(&prepared)?;
            let free = written.keys().all(|path| !taken.contains(path));
            match free && self.stands_alone(&prepared)? {
                true => {
                    taken.extend(written.keys().cloned());
                    plan.members.push(Member { lane: *lane, written });
                }
                false => plan.alone.push(*lane),
            }
        }
        if plan.members.len() < 2 {
            // Nobody to be checked with.
            plan.alone.extend(plan.members.drain(..).map(|member| member.lane));
            plan.alone.sort();
            return Ok(plan);
        }
        // The main lane with what each of them writes.
        let mut entries: std::collections::BTreeMap<PathBuf, TreeEntry> =
            self.main.tree.iter().map(|(path, entry)| (path.to_owned(), *entry)).collect();
        for member in &plan.members {
            for (path, entry) in &member.written {
                match entry {
                    Some(entry) => entries.insert(path.clone(), *entry),
                    None => entries.remove(path),
                };
            }
        }
        let mut tree = Tree::new();
        for (path, entry) in entries {
            tree.apply(&kito_core::OpKind::FileAdd { path, blob: entry.blob, mode: entry.mode })?;
        }
        let confinement = self.confinement()?;
        self.store.commit()?;
        let dir = self.root.join(STORE_DIR).join("verify").join("work");
        self.materialize_tree(&tree, &dir)?;
        plan.checks = Some(Checks {
            dir,
            commands: self.config.verify.commands.clone(),
            timeout: Duration::from_secs(self.config.verify.timeout_secs.max(1)),
            confinement,
        });
        Ok(plan)
    }

    /// Integrates a lane that was checked together with others, if what
    /// its merge writes now, on the main lane as the lanes before it left
    /// it, is what was checked. `None`: it is not (the lane or the main
    /// lane moved, or the merge came out otherwise): the lane has to be
    /// integrated by itself, with checks of its own.
    pub fn land_member(
        &mut self,
        member: &Member,
        report: VerificationReport,
        approved_by: Option<Author>,
    ) -> Result<Option<IntegrationResult>> {
        let prepared = match self.prepare_integration(member.lane, None)? {
            Preparation::Ready(prepared) => prepared,
            _ => return Ok(None),
        };
        if self.written_by(&prepared)? != member.written || !self.stands_alone(&prepared)? {
            return Ok(None);
        }
        self.finish_integration_for(*prepared, Some(report), None, approved_by)
    }

    /// Integrates several lanes at once, with the checks run in place:
    /// what the daemon does for lanes that ask at the same time. The
    /// results come in the order the lanes got them.
    pub fn integrate_together(&mut self, lanes: &[LaneId]) -> Result<Vec<(LaneId, IntegrationResult)>> {
        struct Here<'a> {
            repo: std::cell::RefCell<&'a mut Repo>,
            results: std::cell::RefCell<Vec<(LaneId, IntegrationResult)>>,
        }
        impl Bench for Here<'_> {
            fn repo<R>(&self, f: impl FnOnce(&mut Repo) -> Result<R>) -> Result<R> {
                f(&mut self.repo.borrow_mut())
            }
            fn check(&self, checks: &Checks) -> VerificationReport {
                match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime.block_on(checks.run()),
                    Err(e) => VerificationReport {
                        commands: Vec::new(),
                        passed: false,
                        duration_ms: 0,
                        failed_command: None,
                        output: Some(format!("starting the verifier: {e}")),
                        confined: false,
                        batch: Vec::new(),
                    },
                }
            }
            fn alone(&self, lane: LaneId) -> IntegrationResult {
                let done = self.repo.borrow_mut().integrate_leaving_index(lane);
                done.unwrap_or_else(|e| IntegrationResult::failed(lane.to_string(), e.to_string()))
            }
            fn approver(&self, _lane: LaneId) -> Option<Author> {
                None
            }
            fn done(&self, lane: LaneId, result: IntegrationResult) {
                self.results.borrow_mut().push((lane, result));
            }
        }
        let here = Here { repo: std::cell::RefCell::new(self), results: Default::default() };
        integrate_batch(&here, lanes);
        let results = here.results.into_inner();
        here.repo.into_inner().refresh_git_index()?;
        Ok(results)
    }

    /// How the checks of this repository run: confined, unless the
    /// repository says otherwise or the system cannot. An error if they
    /// must be confined and cannot be.
    pub fn confinement(&self) -> Result<Confinement> {
        use crate::config::SandboxMode;
        let config = &self.config.sandbox;
        if config.mode == SandboxMode::Off {
            return Ok(Confinement::default());
        }
        match crate::sandbox::helper().filter(|_| crate::sandbox::available()) {
            Some(helper) => {
                let policy = crate::sandbox::for_checks(&self.root, config);
                let network = match config.network {
                    true => "",
                    false => " and open no network connection",
                };
                Ok(Confinement {
                    wrapper: crate::sandbox::wrap(&policy, &helper),
                    hint: Some(format!(
                        "the checks run confined: they write only in their own copy of the files{network}; \
                         if that is what failed, see [sandbox] in the configuration of the repository"
                    )),
                })
            }
            None if config.mode == SandboxMode::Required => Err(Error::Other(
                "the checks were not run: the repository requires them to be confined ([sandbox] mode = \
                 \"required\") and this system cannot do it (it needs Linux 5.13 or later, and `kitod`)"
                    .into(),
            )),
            None => Ok(Confinement::default()),
        }
    }

    /// The same, in a word for whoever asks how the repository is kept:
    /// `confined`, `confined, with network`, `off`, `unavailable`.
    pub fn sandbox_state(&self) -> &'static str {
        use crate::config::SandboxMode;
        let able = crate::sandbox::helper().is_some() && crate::sandbox::available();
        match (self.config.sandbox.mode, able, self.config.sandbox.network) {
            (SandboxMode::Off, _, _) => "off",
            (_, false, _) => "unavailable",
            (_, true, true) => "confined, with network",
            (_, true, false) => "confined",
        }
    }

    /// Step 2: materializes the merged result where the verification
    /// commands will run. `None` if no command is configured.
    pub fn verification_copy(&mut self, prepared: &Prepared) -> Result<Option<Checks>> {
        let commands = self.config.verify.commands.clone();
        if commands.is_empty() {
            return Ok(None);
        }
        let confinement = self.confinement()?;
        let tree = self.merged_tree(prepared)?;
        self.store.commit()?;
        let dir = self.root.join(STORE_DIR).join("verify").join("work");
        self.materialize_tree(&tree, &dir)?;
        let timeout = Duration::from_secs(self.config.verify.timeout_secs.max(1));
        Ok(Some(Checks { dir, commands, timeout, confinement }))
    }

    /// Step 3: records the merged result on the main lane, under a new
    /// checkpoint, and brings the lane up to it. `None`: one of the two
    /// lanes moved since the merge was prepared, which must be done again.
    pub fn finish_integration(
        &mut self,
        prepared: Prepared,
        verification: Option<VerificationReport>,
    ) -> Result<Option<IntegrationResult>> {
        self.finish_integration_for(prepared, verification, None, None)
    }

    /// The same, for a person a server knows. One who may not integrate
    /// (`hold`): when the merge composed and the checks passed, nothing is
    /// written and the lane is marked as waiting for somebody who may. One
    /// who may (`approved_by`): the checkpoint says who let the lane in.
    pub fn finish_integration_for(
        &mut self,
        prepared: Prepared,
        verification: Option<VerificationReport>,
        hold: Option<Author>,
        approved_by: Option<Author>,
    ) -> Result<Option<IntegrationResult>> {
        let lane = prepared.lane;
        // Nothing may have been saved meanwhile in the files about to be
        // written: on the main lane those of the merge, in the lane's
        // working copy those in which it differs from the result.
        let written: Vec<PathBuf> = prepared.outcome.files.keys().cloned().collect();
        self.capture_main_under(lane, &written)?;
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let mut rewritten: Vec<PathBuf> =
            at.work.tree.differences(&self.main.tree).map(Path::to_owned).collect();
        rewritten.extend(written);
        self.capture_lane(lane, rewritten)?;
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        if self.main.head != prepared.main_head || at.work.ops != prepared.lane_ops {
            return Ok(None);
        }
        let mut result = IntegrationResult::new(lane.to_string(), Outcome::Integrated);
        if let Some(report) = &verification {
            if !report.passed {
                result.result = Outcome::VerificationFailed;
                result.message = Some(format!(
                    "`{}` failed on the merged result: nothing was integrated",
                    report.failed_command.clone().unwrap_or_default()
                ));
                result.verification = verification;
                return Ok(Some(result));
            }
        }
        if let Some(by) = hold {
            let at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64);
            let (ops, state) = (at.work.ops, at.record.state);
            let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
            at.record.proposed = Some(kito_core::Proposal { by, at_ms, ops });
            self.put_lane_state(lane, state)?;
            self.store.commit()?;
            result.result = Outcome::AwaitingApproval;
            result.message = Some(format!(
                "the merge composes and the checks pass, but integrating is not for who asked: the lane                  waits for somebody who may (`kito integrate {lane}`)"
            ));
            result.verification = verification;
            return Ok(Some(result));
        }
        let (agent, lane_head) = (at.record.agent.clone(), at.work.head);
        let what = at.record.task.clone().or_else(|| {
            at.work.intent.and_then(|id| self.store.intent(&id)).map(|intent| intent.summary.clone())
        });

        // An untracked file where one must be written is not ours to overwrite.
        for (path, file) in &prepared.outcome.files {
            if file.is_some()
                && !self.main.tree.contains(path)
                && fs::symlink_metadata(self.root.join(path)).is_ok()
            {
                return Ok(Some(IntegrationResult::failed(
                    lane.to_string(),
                    format!(
                        "{}: exists in the main directory but is not tracked; move it away",
                        path.display()
                    ),
                )));
            }
        }
        // The files, then the ops that record them: signed by the agent,
        // the first one with the lane's head as second parent.
        let root = self.root.clone();
        let mut written = Vec::new();
        for (path, file) in &prepared.outcome.files {
            match file {
                Some(file) => {
                    let blob = match self.main.tree.get(path) {
                        Some(before) => self.store.put_blob_like(&file.content, &before.blob)?,
                        None => self.store.put_blob(&file.content)?,
                    };
                    written.push((path.clone(), TreeEntry { blob, mode: file.mode }));
                }
                None => remove_in(&root, path)?,
            }
        }
        self.write_files_in(&root, &written)?;
        let summary = match &what {
            Some(what) => format!("integrate {lane}: {what}"),
            None => format!("integrate {lane} ({})", agent.name),
        };
        let intent = Intent::new(summary, self.lanes[&lane].record.task.clone(), None)?;
        self.store.put_intent(&intent)?;
        let paths: Vec<PathBuf> = prepared.outcome.files.keys().cloned().collect();
        let identities = prepared.outcome.identities.clone();
        let signer = agent.clone();
        let ops = self.with(LaneId::MAIN, |repo, work| {
            let human = std::mem::replace(&mut work.author, signer);
            work.merge_parent = lane_head;
            // This is the one way into a protected main lane.
            repo.main_open = true;
            let recorded = repo.capture_work(work, paths, Some(intent.id), &identities, Unsettled::Trust);
            repo.main_open = false;
            work.merge_parent = None;
            work.author = human;
            recorded
        })?;
        let head = self.main.head.ok_or(Error::NothingToCheckpoint)?;

        let label = what.unwrap_or_else(|| format!("{lane} ({})", agent.name));
        let checkpoint = Checkpoint {
            id: self.store.next_checkpoint_id(),
            head,
            label,
            created_by: agent,
            verification: verification.as_ref().map(|report| Verification {
                commands: report.commands.clone(),
                passed: report.passed,
                duration_ms: report.duration_ms,
            }),
            git_commit: None,
            approved_by,
        };
        self.store.put_checkpoint(&checkpoint)?;
        for id in &prepared.settled {
            if let Some(mut conflict) = self.store.conflict(*id).cloned() {
                conflict.resolved_by = ops.first().map(|op| op.id).or(Some(head));
                self.store.put_conflict(&conflict)?;
            }
        }
        if self.config.git.export_on_checkpoint {
            // The commit is made now; git's index, which costs in proportion
            // to the repository, is rewritten once the lane has its answer.
            match self.export_git_later_index() {
                Ok(_) => {
                    result.git_commit = self
                        .store
                        .checkpoint(checkpoint.id)
                        .and_then(|c| c.git_commit.as_ref())
                        .map(|oid| oid.iter().map(|b| format!("{b:02x}")).collect());
                }
                Err(e) => result.message = Some(format!("integrated, but the export to git failed: {e}")),
            }
        }
        self.fast_forward(lane, checkpoint.id)?;
        if !prepared.problems.is_empty() && result.message.is_none() {
            result.message = Some(prepared.problems.join("; "));
        }

        result.auto_resolutions = prepared
            .outcome
            .merged_symbols
            .iter()
            .map(|id| AutoResolution {
                kind: "body_merge".to_owned(),
                symbol: symbol_ref(&self.main.symbols, *id),
                new_name: None,
                locations: self
                    .main
                    .symbols
                    .get(*id)
                    .map(|entry| entry.file.display().to_string())
                    .into_iter()
                    .collect(),
            })
            .chain(prepared.propagated.iter().cloned())
            .collect();
        result.checkpoint = Some(checkpoint.id.to_string());
        result.your_next_base = Some(checkpoint.id.to_string());
        result.verification = verification;
        Ok(Some(result))
    }

    /// Brings a lane to the current state of the main lane: its working
    /// copy, its state and its base. Its ops so far are behind it.
    fn fast_forward(&mut self, lane: LaneId, base: kito_core::CheckpointId) -> Result<()> {
        let main_head = self.main.head.ok_or(Error::NothingToCheckpoint)?;
        let (tree, symbols) = (self.main.tree.clone(), self.main.symbols.clone());
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let workdir = at.work.root.clone();
        let mut write: Vec<(PathBuf, TreeEntry)> = Vec::new();
        let mut remove: Vec<PathBuf> = Vec::new();
        for path in tree.differences(&at.work.tree) {
            match tree.get(path) {
                Some(entry) => write.push((path.to_owned(), *entry)),
                None => remove.push(path.to_owned()),
            }
        }
        for path in &remove {
            remove_in(&workdir, path)?;
        }
        self.write_files_in(&workdir, &write)?;
        let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
        for path in remove.iter().chain(write.iter().map(|(path, _)| path)) {
            at.work.stat.remove(path);
        }
        at.record.next_symbol = at.record.next_symbol.max(at.work.symbols.next_seq(lane));
        at.work.tree = tree.clone();
        at.work.symbols = symbols.clone();
        at.work.symbols.reserve(lane, at.record.next_symbol);
        at.work.last_clean.clear();
        at.work.gone.clear();
        at.work.head = Some(main_head);
        at.base_tree = tree;
        at.base_symbols = symbols;
        at.record.base = base;
        at.record.base_head = main_head;
        at.record.since = at.work.ops;
        at.record.proposed = None;
        // The saved state of the lane is written when the repository is
        // closed: until then the log has everything.
        self.put_lane_state(lane, LaneState::Active)
    }

    /// The three steps in a row, running the verification commands in
    /// place. The daemon does the same without holding the repository
    /// during the verification.
    pub fn integrate(&mut self, lane: LaneId) -> Result<IntegrationResult> {
        let result = self.integrate_leaving_index(lane);
        self.refresh_git_index()?;
        result
    }

    /// [`Repo::integrate`] without the last step, the rewriting of git's
    /// index: for who measures an integration as the daemon does it, with
    /// the index rewritten after the lane has its answer.
    pub fn integrate_leaving_index(&mut self, lane: LaneId) -> Result<IntegrationResult> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Other(format!("starting the verifier: {e}")))?;
        let mut answers = None;
        for _ in 0..6 {
            let prepared = match self.prepare_integration(lane, answers.as_ref())? {
                Preparation::Done(result) => return Ok(*result),
                Preparation::Ready(prepared) => *prepared,
                // What the providers say of the uses of what was renamed.
                Preparation::Ask(plan) => {
                    let semantic = self.semantic();
                    answers = Some(runtime.block_on(crate::refs::answer(&plan, &semantic)));
                    continue;
                }
            };
            let verification =
                self.verification_copy(&prepared)?.map(|checks| runtime.block_on(checks.run()));
            if let Some(result) = self.finish_integration(prepared, verification)? {
                return Ok(result);
            }
        }
        Ok(IntegrationResult::failed(
            lane.to_string(),
            "the lanes keep changing: try again when they are quiet",
        ))
    }
}

/// A symbol for people and logs: `ts/auth.getUser#42`.
pub(crate) fn symbol_ref(table: &SymbolTable, id: SymbolId) -> String {
    let Some(entry) = table.get(id) else { return id.to_string() };
    let stem = entry.file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let extension = entry.file.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let name = table.qualified_name(id).unwrap_or_else(|| entry.name.clone());
    let number = match id.origin().is_main() {
        true => id.seq().to_string(),
        false => format!("{}@{}", id.seq(), id.origin()),
    };
    format!("{extension}/{stem}.{name}#{number}")
}
