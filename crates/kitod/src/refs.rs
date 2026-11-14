//! Level 2 in an integration: a rename made on one side reaches the uses the
//! other side has of the symbol.
//!
//! The lane renamed `getUser` to `fetchUser` while the main lane got a new
//! call to `getUser` (or the other way round). Line by line, and symbol by
//! symbol, nothing conflicts; the result does not compile. So before the
//! merge the providers are asked who uses the symbol, at the common
//! starting point and on the side that did not rename, and on copies of
//! the three states those uses are written with the new name: where the
//! provider found them, and on the renaming side where a line it had at
//! the start is still as it was (a use its author forgot). The merge then
//! composes three states that agree on the name, and what is left to merge
//! is what each side really did.
//!
//! Who uses a symbol is a question for the providers of `kito-semantic`,
//! and a language server may take seconds to answer: the question is
//! prepared while the repository is held ([`Repo::plan_references`]), asked
//! without holding it ([`answer`]), and its answers are applied by the merge
//! only if both lanes are still where they were.
//!
//! A use the provider is sure of (a language server's answer) is rewritten.
//! One it only suspects (the syntax alone) is rewritten only if it is new on
//! its side, and only after someone decided: until then it is a
//! `SemanticRef` conflict.

use crate::error::{Error, Result};
use crate::repo::Repo;
use kito_core::{BlobId, ConflictKind, LaneId, OpId, SymbolId, SymbolTable, Tree};
use kito_integrate::AutoResolution;
use kito_merge::{Found, Subject};
use kito_semantic::{Files, Semantic, Snapshot, Usage, SURE};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One of the two sides of an integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Main,
    Lane,
}

/// A symbol one side renamed and the other still calls by its old name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renamed {
    pub symbol: SymbolId,
    pub by: Side,
    /// The names as they are written: for `Server.Start`, `Start`.
    pub from: String,
    pub to: String,
}

/// A state of the repository that can be asked about when the repository is
/// no longer held: the files of the main directory, and what this state has
/// that differs from them.
struct Frozen {
    root: PathBuf,
    tree: Tree,
    symbols: SymbolTable,
    overlay: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl Files for Frozen {
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        match self.overlay.get(path) {
            Some(content) => content.clone(),
            None => std::fs::read(self.root.join(path)).ok(),
        }
    }
}

impl Frozen {
    fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            root: &self.root,
            tree: &self.tree,
            symbols: &self.symbols,
            files: self,
            overlay: &self.overlay,
        }
    }
}

/// Where the two lanes were when a question was prepared.
type At = (Option<OpId>, u64);

/// What has to be asked before a lane is merged.
pub struct RefPlan {
    at: At,
    renames: Vec<Renamed>,
    base: Frozen,
    main: Frozen,
    lane: Frozen,
}

/// Who uses a renamed symbol.
pub struct Uses {
    pub renamed: Renamed,
    /// On the side that did not rename.
    pub there: Vec<Usage>,
    /// At the common starting point.
    pub at_base: Vec<Usage>,
}

/// What the providers answered.
pub struct RefAnswers {
    at: At,
    pub found: Vec<Uses>,
    /// Providers that failed, and whose place another took.
    pub problems: Vec<String>,
}

/// The part of a name that is written where the symbol is defined, and what
/// comes before it.
fn written(name: &str) -> (&str, &str) {
    match name.rfind(['.', ':']) {
        Some(at) => (&name[..=at], &name[at + 1..]),
        None => ("", name),
    }
}

/// The symbols one side renamed and the other did not touch the name of.
fn renames(base: &SymbolTable, main: &SymbolTable, lane: &SymbolTable) -> Vec<Renamed> {
    let mut found = Vec::new();
    for (by, side, other) in [(Side::Main, main, lane), (Side::Lane, lane, main)] {
        for id in base.changed(side) {
            let (Some(old), Some(new), Some(theirs)) = (base.get(id), side.get(id), other.get(id)) else {
                continue;
            };
            let ((owner, from), (new_owner, to)) = (written(&old.name), written(&new.name));
            // Renamed on both sides is for the merge to judge; a name that
            // changed only in its owner is the owner's rename.
            if theirs.name == old.name && owner == new_owner && from != to {
                found.push(Renamed { symbol: id, by, from: from.to_owned(), to: to.to_owned() });
            }
        }
    }
    found
}

/// Where a name is replaced in a content, and by what.
type Replacement = (std::ops::Range<usize>, String);

/// What the rewriting of the uses came to.
pub(crate) struct Propagation {
    /// The three states as the merge must see them: `None`, as they are.
    pub base: Option<Tree>,
    pub main: Option<Tree>,
    pub lane: Option<Tree>,
    /// Uses that may be of a renamed symbol: for someone to decide.
    pub doubts: Vec<Found>,
    pub done: Vec<AutoResolution>,
}

impl RefAnswers {
    /// Which side renamed `symbol`, if these answers are about it.
    pub(crate) fn renamed_by(&self, symbol: SymbolId) -> Option<Side> {
        self.found.iter().find(|uses| uses.renamed.symbol == symbol).map(|uses| uses.renamed.by)
    }
}

/// Asks the providers who uses each renamed symbol, where it still has its
/// old name. The repository is not needed.
pub async fn answer(plan: &RefPlan, semantic: &Semantic) -> RefAnswers {
    let mut answers = RefAnswers { at: plan.at, found: Vec::new(), problems: Vec::new() };
    for renamed in &plan.renames {
        let there = match renamed.by {
            Side::Main => &plan.lane,
            Side::Lane => &plan.main,
        };
        let mut asked = Vec::new();
        for state in [there, &plan.base] {
            let found = semantic.usages(&state.snapshot(), renamed.symbol, &mut answers.problems).await;
            asked.push(found.unwrap_or_else(|e| {
                answers.problems.push(e.to_string());
                Vec::new()
            }));
        }
        let at_base = asked.pop().unwrap_or_default();
        let there = asked.pop().unwrap_or_default();
        answers.found.push(Uses { renamed: renamed.clone(), there, at_base });
    }
    answers.problems.sort();
    answers.problems.dedup();
    answers
}

/// The state of a lane in which a symbol is about to be renamed.
pub struct RenamePlan {
    ops: u64,
    pub symbol: SymbolId,
    state: Frozen,
}

/// Who uses a symbol about to be renamed.
pub struct RenameUses {
    pub(crate) ops: u64,
    pub(crate) symbol: SymbolId,
    pub uses: Vec<Usage>,
    pub problems: Vec<String>,
}

/// Asks the providers who uses the symbol. The repository is not needed.
pub async fn uses_of(plan: &RenamePlan, semantic: &Semantic) -> RenameUses {
    let mut problems = Vec::new();
    let uses =
        semantic.usages(&plan.state.snapshot(), plan.symbol, &mut problems).await.unwrap_or_else(|e| {
            problems.push(e.to_string());
            Vec::new()
        });
    RenameUses { ops: plan.ops, symbol: plan.symbol, uses, problems }
}

/// Start, end and number (from 1) of the line a byte is on.
fn line_of(content: &[u8], at: usize) -> (usize, usize, usize) {
    let start = content[..at].iter().rposition(|&b| b == b'\n').map_or(0, |n| n + 1);
    let end = content[start..].iter().position(|&b| b == b'\n').map_or(content.len(), |n| start + n);
    (start, end, 1 + content[..start].iter().filter(|&&b| b == b'\n').count())
}

impl Repo {
    /// A state of the repository, for the providers.
    fn frozen(&self, tree: &Tree, symbols: &SymbolTable) -> Result<Frozen> {
        let mut overlay = BTreeMap::new();
        for path in tree.differences(&self.main.tree) {
            let content = match tree.get(path) {
                Some(entry) => Some(self.store.get_blob(&entry.blob)?.unwrap_or_default()),
                None => None,
            };
            overlay.insert(path.to_owned(), content);
        }
        Ok(Frozen { root: self.root.clone(), tree: tree.clone(), symbols: symbols.clone(), overlay })
    }

    /// What to ask before renaming a symbol of `lane` (given as for
    /// [`Repo::rename_symbol`]): its uses, to rename them with it.
    pub fn plan_rename(&mut self, lane: LaneId, symbol: &str) -> Result<RenamePlan> {
        self.scan_lane(lane)?;
        let symbol = self.symbol_of(lane, symbol)?;
        let work = self.work(lane)?;
        Ok(RenamePlan { ops: work.ops, symbol, state: self.frozen(&work.tree, &work.symbols)? })
    }

    /// What level 2 has to find out before `lane` is merged. `None`: neither
    /// side renamed anything the other still calls by its old name, which is
    /// almost always.
    pub fn plan_references(&mut self, lane: LaneId) -> Result<Option<RefPlan>> {
        if lane.is_main() {
            return Ok(None);
        }
        self.scan_lane(lane)?;
        self.capture_main_under(lane, &[])?;
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let renames = renames(&at.base_symbols, &self.main.symbols, &at.work.symbols);
        if renames.is_empty() {
            return Ok(None);
        }
        Ok(Some(RefPlan {
            at: (self.main.head, at.work.ops),
            renames,
            base: self.frozen(&at.base_tree, &at.base_symbols)?,
            main: self.frozen(&self.main.tree, &self.main.symbols)?,
            lane: self.frozen(&at.work.tree, &at.work.symbols)?,
        }))
    }

    /// `true` if one side renamed something the other still calls by its
    /// old name: the providers have to be asked before merging.
    pub(crate) fn has_renames(&self, lane: LaneId) -> bool {
        self.lanes
            .get(&lane)
            .is_some_and(|at| !renames(&at.base_symbols, &self.main.symbols, &at.work.symbols).is_empty())
    }

    /// `true` if these answers were given for the lanes as they are now.
    pub(crate) fn answers_hold(&self, lane: LaneId, answers: &RefAnswers) -> bool {
        self.lanes.get(&lane).is_some_and(|at| answers.at == (self.main.head, at.work.ops))
    }

    /// Writes with their new names, on copies of the three states, the uses
    /// of the symbols one side renamed. `decided`: for the symbols with
    /// doubtful uses, whether to rewrite them (`true`) or leave them.
    pub(crate) fn propagate(
        &mut self,
        lane: LaneId,
        answers: &RefAnswers,
        decided: &HashMap<SymbolId, bool>,
    ) -> Result<Propagation> {
        const BASE: usize = 0;
        const MAIN: usize = 1;
        const LANE: usize = 2;
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let base_symbols = at.base_symbols.clone();
        let before = [at.base_tree.clone(), self.main.tree.clone(), at.work.tree.clone()];
        let read = |repo: &Repo, state: usize, file: &Path| -> Result<Option<Vec<u8>>> {
            match before[state].get(file) {
                Some(entry) => Ok(Some(repo.store.get_blob(&entry.blob)?.unwrap_or_default())),
                None => Ok(None),
            }
        };
        // Per state and file: where a name is replaced, and by what. All the
        // places are in the contents as they are now.
        let mut edits: BTreeMap<(usize, PathBuf), Vec<Replacement>> = BTreeMap::new();
        let mut out =
            Propagation { base: None, main: None, lane: None, doubts: Vec::new(), done: Vec::new() };

        for uses in &answers.found {
            let renamed = &uses.renamed;
            let (renamer, user) = match renamed.by {
                Side::Main => (MAIN, LANE),
                Side::Lane => (LANE, MAIN),
            };
            let name = renamed.from.as_bytes();
            let mut carried: Vec<String> = Vec::new();
            let mut doubtful: Vec<String> = Vec::new();

            // What the provider is sure of at the starting point takes the
            // new name there, and on the side that renamed wherever a line
            // is still as it was: a use the rename did not reach.
            let mut at_base: BTreeMap<&Path, Vec<&Usage>> = BTreeMap::new();
            for usage in uses.at_base.iter().filter(|usage| usage.confidence >= SURE) {
                at_base.entry(&usage.file).or_default().push(usage);
            }
            for (file, sites) in at_base {
                let Some(content) = read(self, BASE, file)? else { continue };
                let theirs = read(self, renamer, file)?;
                for usage in sites {
                    if content.get(usage.range.clone()) != Some(name) {
                        continue;
                    }
                    edits
                        .entry((BASE, file.to_owned()))
                        .or_default()
                        .push((usage.range.clone(), renamed.to.clone()));
                    let Some(theirs) = &theirs else { continue };
                    let (start, end, _) = line_of(&content, usage.range.start);
                    let (line, column) = (&content[start..end], usage.range.start - start);
                    let mut from = 0;
                    for (number, other) in theirs.split(|&b| b == b'\n').enumerate() {
                        let place = from + column..from + column + name.len();
                        if other == line && theirs.get(place.clone()) == Some(name) {
                            edits
                                .entry((renamer, file.to_owned()))
                                .or_default()
                                .push((place, renamed.to.clone()));
                            carried.push(format!("{}:{}", file.display(), number + 1));
                        }
                        from += other.len() + 1;
                    }
                }
            }

            // The uses on the side that did not rename.
            let mut there: BTreeMap<&Path, Vec<&Usage>> = BTreeMap::new();
            for usage in &uses.there {
                there.entry(&usage.file).or_default().push(usage);
            }
            for (file, sites) in there {
                let Some(content) = read(self, user, file)? else { continue };
                let start_of = read(self, BASE, file)?.unwrap_or_default();
                let known: HashSet<&[u8]> = start_of.split(|&b| b == b'\n').collect();
                for usage in sites {
                    if content.get(usage.range.clone()) != Some(name) {
                        continue;
                    }
                    let (start, end, number) = line_of(&content, usage.range.start);
                    // New on this side: a line the starting point did not have.
                    let new = !known.contains(&content[start..end]);
                    let place = format!("{}:{number}", file.display());
                    let rewrite = match usage.confidence >= SURE {
                        true => true,
                        false if !new => false,
                        false => match decided.get(&renamed.symbol) {
                            Some(rewrite) => *rewrite,
                            None => {
                                let line = String::from_utf8_lossy(content[start..end].trim_ascii());
                                doubtful.push(format!("{place}: {line}"));
                                false
                            }
                        },
                    };
                    if rewrite {
                        edits
                            .entry((user, file.to_owned()))
                            .or_default()
                            .push((usage.range.clone(), renamed.to.clone()));
                        if new {
                            carried.push(place);
                        }
                    }
                }
            }

            let table = match renamed.by {
                Side::Main => &self.lanes[&lane].work.symbols,
                Side::Lane => &self.main.symbols,
            };
            if !doubtful.is_empty() {
                doubtful.sort();
                doubtful.dedup();
                let said = format!("renamed: {} → {}\n", renamed.from, renamed.to).into_bytes();
                let listed = format!(
                    "uses of `{}` that may be of this symbol:\n{}\n",
                    renamed.from,
                    doubtful.join("\n")
                )
                .into_bytes();
                let (main, lane) = match renamed.by {
                    Side::Main => (said, listed),
                    Side::Lane => (listed, said),
                };
                out.doubts.push(Found {
                    kind: ConflictKind::SemanticRef,
                    subject: Subject::Symbol(renamed.symbol),
                    file: table.get(renamed.symbol).map(|entry| entry.file.clone()).unwrap_or_default(),
                    name: table.qualified_name(renamed.symbol),
                    base: Some(renamed.from.clone().into_bytes()),
                    main: Some(main),
                    lane: Some(lane),
                });
            }
            if !carried.is_empty() {
                carried.sort();
                carried.dedup();
                out.done.push(AutoResolution {
                    kind: "rename_propagation".to_owned(),
                    symbol: crate::integrate::symbol_ref(&base_symbols, renamed.symbol),
                    new_name: Some(renamed.to.clone()),
                    locations: carried,
                });
            }
        }

        let mut after = before.clone();
        for ((state, file), mut places) in edits {
            let Some(mut text) = read(self, state, &file)? else { continue };
            places.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
            places.dedup_by_key(|(range, _)| range.start);
            for (range, name) in places {
                text.splice(range, name.bytes());
            }
            let blob: BlobId = self.store.put_blob(&text)?;
            after[state].replace(&file, blob)?;
        }
        let [base, main, lane_tree] = after;
        out.base = (base != before[BASE]).then_some(base);
        out.main = (main != before[MAIN]).then_some(main);
        out.lane = (lane_tree != before[LANE]).then_some(lane_tree);
        Ok(out)
    }
}
