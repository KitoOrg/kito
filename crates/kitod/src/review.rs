//! Review of a lane (ADR 0004): what it changes, file by file and
//! function by function.
//!
//! The diff is the lane as it is against what it started from. A function
//! that was renamed or moved is one change with its two names, not a
//! removal and an addition: its identity is in the history.

use crate::error::{Error, Result};
use crate::lanes::lane_word;
use crate::repo::Repo;
use kito_core::{LaneId, OpKind, SymbolId, SymbolTable};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Lines shown around a change.
const CONTEXT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Added,
    Changed,
    Removed,
    /// Under another name, or in another file: `was` says which.
    Renamed,
}

impl Change {
    pub fn word(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Changed => "changed",
            Change::Removed => "removed",
            Change::Renamed => "renamed",
        }
    }
}

/// Lines that changed, with the lines around them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// The first line of the hunk before, from 1, and how many it has.
    pub before: (u32, u32),
    pub after: (u32, u32),
    /// Each line after ` ` (as it was), `-` (gone) or `+` (new).
    pub lines: Vec<String>,
}

/// What a lane did to a function or a class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolChange {
    /// `src/shop.ts:total`.
    pub symbol: String,
    pub change: Change,
    /// What it was called, and where, when that changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was: Option<String>,
    /// The lines it is on in the lane's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<(u32, u32)>,
    /// Why, as the lane's ops on it say.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intents: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    /// Its name before, if the lane renamed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was: Option<String>,
    pub change: Change,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    /// The IDs of its contents: `get_blobs` gives them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Lines the lane added and removed.
    pub added: usize,
    pub removed: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<Hunk>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<SymbolChange>,
}

/// What a lane changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneDiff {
    pub lane: String,
    /// Where it lands: `main`, `lane/3`.
    pub onto: String,
    /// The checkpoint it is based on.
    pub base: String,
    /// Files its target changed since the lane started from it: what the
    /// lane has not seen yet.
    pub behind: usize,
    pub files: Vec<FileDiff>,
}

/// The lines of a text as a diff shows them, and whether the last one ends.
fn lines_of(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes).lines().map(str::to_owned).collect()
}

/// The hunks between two texts, and how many lines were added and removed.
fn hunks(before: &[u8], after: &[u8]) -> (Vec<Hunk>, usize, usize) {
    let (old, new) = (lines_of(before), lines_of(after));
    // Every line of the two, in order: its mark, and where it is on each side.
    let mut rows: Vec<(char, usize, usize, &str)> = Vec::with_capacity(old.len().max(new.len()));
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, &old, &new) {
        let (tag, was, now) = op.as_tag_tuple();
        match tag {
            similar::DiffTag::Equal => {
                rows.extend(was.zip(now).map(|(a, b)| (' ', a, b, old[a].as_str())));
            }
            similar::DiffTag::Delete | similar::DiffTag::Insert | similar::DiffTag::Replace => {
                let (at_old, at_new) = (was.start, now.start);
                rows.extend(was.clone().map(|a| ('-', a, at_new, old[a].as_str())));
                rows.extend(now.map(|b| ('+', at_old + was.len(), b, new[b].as_str())));
            }
        }
    }
    let added = rows.iter().filter(|row| row.0 == '+').count();
    let removed = rows.iter().filter(|row| row.0 == '-').count();
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(first) = (at..rows.len()).find(|&i| rows[i].0 != ' ') {
        let start = first.saturating_sub(CONTEXT);
        // The hunk goes on while the next change is within its context.
        let mut last = first;
        while let Some(next) = (last + 1..rows.len().min(last + 2 * CONTEXT + 2)).find(|&i| rows[i].0 != ' ')
        {
            last = next;
        }
        let end = (last + CONTEXT + 1).min(rows.len());
        let part = &rows[start..end];
        let count = |skip: char| part.iter().filter(|row| row.0 != skip).count() as u32;
        // As diffs are written: a side with no lines is said to be after
        // the line before it ( for a new file).
        let side = |at: usize, lines: u32| (at as u32 + u32::from(lines > 0), lines);
        found.push(Hunk {
            before: side(part[0].1, count('+')),
            after: side(part[0].2, count('-')),
            lines: part.iter().map(|row| format!("{}{}", row.0, row.3)).collect(),
        });
        at = end;
    }
    (found, added, removed)
}

/// What changed in a lane since somebody's last verdict on it (ADR 0019).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesSince {
    pub lane: String,
    /// The verdict they are compared with: `verdict#4`, given then.
    pub verdict: String,
    pub at_ms: u64,
    pub files: Vec<FileDiff>,
}

impl Repo {
    /// What changed in `lane` since `person` last approved it or asked for
    /// changes: each file whose content is not what they saw then.
    pub fn changes_since(&self, lane: LaneId, person: &str) -> Result<ChangesSince> {
        let at = self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))?;
        let verdict = self
            .store
            .verdicts()
            .rev()
            .filter(|verdict| verdict.lane == lane && verdict.kind != kito_core::VerdictKind::Withdrawn)
            .find(|verdict| verdict.person.as_ref().unwrap_or(&verdict.by).name == person)
            .ok_or_else(|| {
                Error::Other(format!(
                    "{person} gave no verdict on {lane}: there is no review to compare with"
                ))
            })?;
        let now = &at.work.tree;
        let mut paths: BTreeSet<PathBuf> = verdict.seen.keys().cloned().collect();
        paths.extend(
            now.iter()
                .filter(|(path, entry)| at.base_tree.get(path) != Some(*entry))
                .map(|(path, _)| path.to_path_buf()),
        );
        let blob =
            |id: &kito_core::BlobId| -> Result<Vec<u8>> { Ok(self.store.get_blob(id)?.unwrap_or_default()) };
        let mut files = Vec::new();
        for path in paths {
            // What they saw: the lane's, or else what it started from.
            let then =
                verdict.seen.get(&path).copied().or_else(|| at.base_tree.get(&path).map(|entry| entry.blob));
            let later = now.get(&path).map(|entry| entry.blob);
            if then == later {
                continue;
            }
            let old = then.as_ref().map(blob).transpose()?.unwrap_or_default();
            let new = later.as_ref().map(blob).transpose()?.unwrap_or_default();
            let binary = kito_diff::text::is_binary(&old) || kito_diff::text::is_binary(&new);
            let (hunks, added, removed) = match binary {
                true => (Vec::new(), 0, 0),
                false => hunks(&old, &new),
            };
            files.push(FileDiff {
                path: shown(&path),
                was: None,
                change: match (then, later) {
                    (None, _) => Change::Added,
                    (_, None) => Change::Removed,
                    _ => Change::Changed,
                },
                binary,
                before: then.map(|id| id.to_string()),
                after: later.map(|id| id.to_string()),
                added,
                removed,
                hunks,
                symbols: Vec::new(),
            });
        }
        Ok(ChangesSince {
            lane: lane.to_string(),
            verdict: verdict.id.to_string(),
            at_ms: verdict.at_ms,
            files,
        })
    }
}

/// A path as Kito names files: with `/`.
fn shown(path: &Path) -> String {
    kito_core::paths::portable_text(&path.display().to_string())
}

/// `file:Name.method`, as a table has it.
fn label(table: &SymbolTable, id: SymbolId) -> Option<String> {
    let entry = table.get(id)?;
    let name = table.qualified_name(id).unwrap_or_else(|| entry.name.clone());
    Some(format!("{}:{name}", shown(&entry.file)))
}

/// The symbol an op is about.
pub(crate) fn symbol_of(kind: &OpKind) -> Option<SymbolId> {
    match kind {
        OpKind::AddSymbol { sym, .. } => Some(sym.id),
        OpKind::RemoveSymbol { id }
        | OpKind::ModifyBody { id, .. }
        | OpKind::ModifySignature { id, .. }
        | OpKind::Rename { id, .. }
        | OpKind::Move { id, .. } => Some(*id),
        OpKind::TextEdit { .. }
        | OpKind::FileAdd { .. }
        | OpKind::FileRemove { .. }
        | OpKind::FileRename { .. } => None,
    }
}

/// How many lines two texts differ by: added, removed.
pub(crate) fn line_counts(before: &[u8], after: &[u8]) -> (usize, usize) {
    let (old, new) = (lines_of(before), lines_of(after));
    let (mut added, mut removed) = (0, 0);
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, &old, &new) {
        let (tag, was, now) = op.as_tag_tuple();
        if tag != similar::DiffTag::Equal {
            removed += was.len();
            added += now.len();
        }
    }
    (added, removed)
}

/// The reasons of a change, without Kito's own `integrate lane/N: …` when
/// the lane's own are there: it only says them again.
fn own_reasons(said: Vec<String>) -> Vec<String> {
    let kitos = |words: &String| crate::history::integrate_of(words).is_some();
    match said.iter().all(kitos) {
        true => said,
        false => said.into_iter().filter(|words| !kitos(words)).collect(),
    }
}

/// One side of a diff: the files and the symbols, as they were then.
#[derive(Clone, Copy)]
pub(crate) struct Side<'a> {
    pub(crate) tree: &'a kito_core::Tree,
    pub(crate) symbols: &'a SymbolTable,
}

/// What the ops between the two sides of a diff say: the files renamed,
/// the symbols touched and why.
#[derive(Default)]
pub(crate) struct Said {
    renamed: BTreeMap<PathBuf, PathBuf>,
    touched: BTreeMap<SymbolId, Vec<String>>,
}

impl Said {
    /// Takes in an op; ops are given oldest first.
    pub(crate) fn take(&mut self, repo: &Repo, op: &kito_core::Op) {
        if let OpKind::FileRename { from, to } = &op.kind {
            let first = self.renamed.remove(from).unwrap_or_else(|| from.clone());
            self.renamed.insert(to.clone(), first);
        }
        if let Some(id) = symbol_of(&op.kind) {
            let said = self.touched.entry(id).or_default();
            if let Some(words) = repo.words(op).filter(|words| !said.iter().any(|w| w == words)) {
                said.push(words.to_owned());
            }
        }
    }
}

impl Repo {
    /// What a lane changes against what it started from; with `paths`, in
    /// those files and directories only. For a closed lane: what it had
    /// changed and not integrated when it closed.
    pub fn lane_diff(&self, lane: LaneId, paths: &[String]) -> Result<LaneDiff> {
        if lane.is_main() {
            return Err(Error::Other(
                "the main lane has no diff: it is what lanes are compared with (`kito log` shows what went in)"
                    .to_owned(),
            ));
        }
        let Some(at) = self.lanes.get(&lane) else {
            return self.closed_lane_diff(lane, paths);
        };
        // What the lane's own ops say.
        let mut said = Said::default();
        self.lane_ops_from(lane, at.record.since, |op| said.take(self, op))?;
        let base = Side { tree: &at.base_tree, symbols: &at.base_symbols };
        let now = Side { tree: &at.work.tree, symbols: &at.work.symbols };
        let files = self.diff_files(base, now, said, paths)?;
        let target = at.record.target_lane();
        let behind = match self.work(target) {
            Ok(work) => work.tree.differences(&at.base_tree).count(),
            Err(_) => 0,
        };
        Ok(LaneDiff {
            lane: lane.to_string(),
            onto: lane_word(target),
            base: at.record.base.to_string(),
            behind,
            files,
        })
    }

    /// The same, for a closed lane: its head against what it was last
    /// brought up to. Nothing is behind a lane that lands nowhere now.
    fn closed_lane_diff(&self, lane: LaneId, paths: &[String]) -> Result<LaneDiff> {
        let record = self.store.lane_record(lane).ok_or(Error::NoSuchLane(lane))?.clone();
        let (base_tree, base_symbols) = self.state_of(record.base_head)?;
        let (tree, symbols) = self.state_of(record.head)?;
        let mut said = Said::default();
        self.lane_ops_from(lane, record.since, |op| said.take(self, op))?;
        let files = self.diff_files(
            Side { tree: &base_tree, symbols: &base_symbols },
            Side { tree: &tree, symbols: &symbols },
            said,
            paths,
        )?;
        Ok(LaneDiff {
            lane: lane.to_string(),
            onto: lane_word(record.target_lane()),
            base: record.base.to_string(),
            behind: 0,
            files,
        })
    }

    /// Calls `f` on the ops of a lane from the `start`-th on, oldest first.
    pub(crate) fn lane_ops_from(
        &self,
        lane: LaneId,
        start: u64,
        mut f: impl FnMut(&kito_core::Op),
    ) -> Result<()> {
        const PAGE: usize = 1024;
        let mut start = start;
        loop {
            let ops = self.store.lane_ops(lane, start, PAGE)?;
            ops.iter().for_each(&mut f);
            if ops.len() < PAGE {
                return Ok(());
            }
            start += ops.len() as u64;
        }
    }

    /// What differs between two states, file by file and symbol by
    /// symbol, as a review shows it; with `paths`, in those files and
    /// directories only.
    pub(crate) fn diff_files(
        &self,
        before: Side<'_>,
        after: Side<'_>,
        said: Said,
        paths: &[String],
    ) -> Result<Vec<FileDiff>> {
        let (base, now) = (before.tree, after.tree);
        let Said { mut renamed, touched } = said;
        // A file renamed and renamed back is not renamed; nor is one whose
        // first name is taken again.
        renamed
            .retain(|to, from| to != from && base.contains(from) && !now.contains(from) && now.contains(to));
        let moved_from: BTreeSet<&Path> = renamed.values().map(PathBuf::as_path).collect();

        let mut differing: BTreeSet<&Path> = BTreeSet::new();
        for (path, entry) in now.iter() {
            if base.get(path) != Some(entry) {
                differing.insert(path);
            }
        }
        for (path, _) in base.iter() {
            if !now.contains(path) && !moved_from.contains(path) {
                differing.insert(path);
            }
        }
        let wanted = |path: &Path| {
            paths.is_empty()
                || paths.iter().any(|asked| {
                    let asked = Path::new(asked.trim_start_matches("./"));
                    asked.as_os_str().is_empty() || path.starts_with(asked)
                })
        };

        // The symbols that differ, under the file each is in now (or was
        // last in).
        let (was_table, table) = (before.symbols, after.symbols);
        let mut ids: BTreeSet<SymbolId> = touched.keys().copied().collect();
        ids.extend(was_table.changed(table));
        let mut by_file: BTreeMap<&Path, Vec<SymbolId>> = BTreeMap::new();
        for id in ids {
            if let Some(entry) = table.get(id).or_else(|| was_table.get(id)) {
                by_file.entry(entry.file.as_path()).or_default().push(id);
            }
        }

        let blob =
            |id: &kito_core::BlobId| -> Result<Vec<u8>> { Ok(self.store.get_blob(id)?.unwrap_or_default()) };
        let mut files = Vec::new();
        for path in differing {
            let first = renamed.get(path);
            if !wanted(path) && !first.is_some_and(|first| wanted(first)) {
                continue;
            }
            let before = base.get(first.map_or(path, PathBuf::as_path));
            let after = now.get(path);
            let change = match (before, after, first) {
                (_, _, Some(_)) => Change::Renamed,
                (None, _, _) => Change::Added,
                (_, None, _) => Change::Removed,
                _ => Change::Changed,
            };
            let old = before.map(|entry| blob(&entry.blob)).transpose()?.unwrap_or_default();
            let new = after.map(|entry| blob(&entry.blob)).transpose()?.unwrap_or_default();
            let binary = kito_diff::text::is_binary(&old) || kito_diff::text::is_binary(&new);
            let (hunks, added, removed) = match binary {
                true => (Vec::new(), 0, 0),
                false => hunks(&old, &new),
            };

            let spans: BTreeMap<SymbolId, (u32, u32)> = match after {
                Some(_) => self
                    .spans_in(now, table, path)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(id, from, to)| (id, (from, to)))
                    .collect(),
                None => BTreeMap::new(),
            };
            let mut symbols = Vec::new();
            for id in by_file.get(path).into_iter().flatten().copied() {
                let (old_name, new_name) = (label(was_table, id), label(table, id));
                let (change, was) = match (&old_name, &new_name) {
                    (None, Some(_)) => (Change::Added, None),
                    (Some(_), None) => (Change::Removed, None),
                    (Some(old), Some(new)) if old != new => {
                        // A function that only follows its file to a new
                        // name is the file's rename, not its own.
                        let follows = first.is_some_and(|first| {
                            was_table.get(id).is_some_and(|entry| &entry.file == first)
                                && was_table.qualified_name(id) == table.qualified_name(id)
                        });
                        match follows {
                            true if !touched.contains_key(&id) => continue,
                            true => (Change::Changed, None),
                            false => (Change::Renamed, Some(old.clone())),
                        }
                    }
                    (Some(_), Some(_)) => (Change::Changed, None),
                    (None, None) => continue,
                };
                let Some(symbol) = new_name.or(old_name) else { continue };
                symbols.push(SymbolChange {
                    symbol,
                    change,
                    was,
                    lines: spans.get(&id).copied(),
                    intents: own_reasons(touched.get(&id).cloned().unwrap_or_default()),
                });
            }
            symbols.sort_by(|a, b| {
                (a.lines.is_none(), a.lines, &a.symbol).cmp(&(b.lines.is_none(), b.lines, &b.symbol))
            });

            files.push(FileDiff {
                path: shown(path),
                was: first.map(|first| shown(first)),
                change,
                binary,
                before: before.map(|entry| entry.blob.to_string()),
                after: after.map(|entry| entry.blob.to_string()),
                added,
                removed,
                hunks,
                symbols,
            });
        }
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: std::ops::Range<u32>) -> String {
        lines.map(|n| format!("line {n}\n")).collect()
    }

    #[test]
    fn a_change_is_shown_with_the_lines_around_it() {
        let before = text(1..21);
        let after = before.replace("line 10\n", "line ten\n");
        let (found, added, removed) = hunks(before.as_bytes(), after.as_bytes());
        assert_eq!((added, removed), (1, 1));
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].before, found[0].after), ((7, 7), (7, 7)));
        assert_eq!(
            found[0].lines,
            [" line 7", " line 8", " line 9", "-line 10", "+line ten", " line 11", " line 12", " line 13"]
        );
    }

    #[test]
    fn changes_far_apart_are_two_hunks_and_near_ones_are_one() {
        let before = text(1..41);
        let far = before.replace("line 5\n", "five\n").replace("line 30\n", "");
        let (found, added, removed) = hunks(before.as_bytes(), far.as_bytes());
        assert_eq!((found.len(), added, removed), (2, 1, 2));
        assert_eq!((found[1].before, found[1].after), ((27, 7), (27, 6)));

        let near = before.replace("line 5\n", "five\n").replace("line 9\n", "nine\n");
        let (found, ..) = hunks(before.as_bytes(), near.as_bytes());
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].before, found[0].after), ((2, 11), (2, 11)));
    }

    #[test]
    fn a_new_file_and_a_removed_one_are_all_lines() {
        let (found, added, removed) = hunks(b"", b"one\ntwo\n");
        assert_eq!((added, removed), (2, 0));
        assert_eq!(
            (found[0].before, found[0].after, &found[0].lines),
            ((0, 0), (1, 2), &vec!["+one".to_owned(), "+two".to_owned()])
        );
        let (found, added, removed) = hunks(b"one\ntwo\n", b"");
        assert_eq!((found.len(), added, removed), (1, 0, 2));
        assert!(hunks(b"same\n", b"same\n").0.is_empty());
    }
}
