//! Explicit renames: an agent says which symbol gets which name, and the
//! differ's heuristics are not asked.

use crate::error::{Error, Result};
use crate::repo::Repo;
use kito_core::{LaneId, SymbolId, TreeEntry};
use kito_diff::ast::Identities;
use kito_parse::{dialect_for_path, parse};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How many places that still use the old name are listed.
const MAX_REFERENCES: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameResult {
    /// The symbol's ID.
    pub symbol: String,
    pub from: String,
    pub to: String,
    pub file: String,
    /// The ops recorded.
    pub ops: Vec<String>,
    /// Places (`file:line`) where the old name still appears as a word,
    /// outside the symbol's definition: they may be uses of it, and were
    /// not touched. Following references is level 2 semantics.
    pub possible_references: Vec<String>,
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

fn is_identifier(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_word) && !name.as_bytes()[0].is_ascii_digit()
}

/// Positions of the whole-word occurrences of `word` in `text`.
fn occurrences(text: &[u8], word: &[u8]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + word.len() <= text.len() {
        let hit = text[i..].starts_with(word)
            && !i.checked_sub(1).is_some_and(|p| is_word(text[p]))
            && !text.get(i + word.len()).is_some_and(|c| is_word(*c));
        if hit {
            found.push(i);
            i += word.len();
        } else {
            i += 1;
        }
    }
    found
}

impl Repo {
    /// Finds a symbol given as `file:name`, as an ID (`1#3`), or as a name
    /// that only one symbol of the lane has.
    fn symbol_of(&self, lane: LaneId, spec: &str) -> Result<SymbolId> {
        let table = &self.work(lane)?.symbols;
        let spec = spec.trim();
        if let Ok(id) = spec.parse::<SymbolId>() {
            return table
                .get(id)
                .map(|_| id)
                .ok_or_else(|| Error::Other(format!("no symbol {id} on {lane}")));
        }
        let found: Vec<SymbolId> = match spec.rsplit_once(':') {
            Some((file, name)) if !file.is_empty() => table.find(Path::new(file), name),
            _ => table
                .iter()
                .filter(|(id, entry)| {
                    entry.name == spec || table.qualified_name(*id).as_deref() == Some(spec)
                })
                .map(|(id, _)| id)
                .collect(),
        };
        match found.as_slice() {
            [id] => Ok(*id),
            [] => Err(Error::Other(format!("no symbol `{spec}` on {lane}: give it as file:name or by ID"))),
            many => Err(Error::Other(format!(
                "`{spec}` is the name of {} symbols on {lane}: give it as file:name or by ID ({})",
                many.len(),
                many.iter()
                    .filter_map(|id| table.get(*id).map(|e| format!(
                        "{}:{} = {id}",
                        e.file.display(),
                        e.name
                    )))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// Renames a symbol in a lane's working copy: its definition and the
    /// uses of its own name inside it. The rename is recorded as such,
    /// whatever the differ would have guessed.
    pub fn rename_symbol(&mut self, lane: LaneId, symbol: &str, new_name: &str) -> Result<RenameResult> {
        let new_name = new_name.trim();
        if !is_identifier(new_name) {
            return Err(Error::Other(format!("{new_name:?} is not a name a symbol can have")));
        }
        self.scan_lane(lane)?;
        let id = self.symbol_of(lane, symbol)?;
        let work = self.work(lane)?;
        let entry = work.symbols.get(id).cloned().ok_or_else(|| Error::Other(format!("no symbol {id}")))?;
        // The name as written in the code: the last word of the recorded
        // one (`Type::method` is recorded whole in some languages).
        let cut = entry.name.bytes().rposition(|c| !is_word(c)).map_or(0, |i| i + 1);
        let (scope, old) = entry.name.split_at(cut);
        if !is_identifier(old) {
            return Err(Error::Other(format!("`{}` cannot be renamed this way", entry.name)));
        }
        if old == new_name {
            return Err(Error::Other(format!("`{old}` already has that name")));
        }
        let tree_entry = *work.tree.get(&entry.file).ok_or_else(|| {
            Error::Other(format!("{}: the file of the symbol is not in the lane", entry.file.display()))
        })?;
        let content = self.store.get_blob(&tree_entry.blob)?.unwrap_or_default();
        let dialect = dialect_for_path(&entry.file)
            .ok_or_else(|| Error::Other(format!("{}: not a language Kito reads", entry.file.display())))?;
        let parsed = parse(dialect, &content).filter(|file| !file.has_errors).ok_or_else(|| {
            Error::Other(format!("{}: has syntax errors, fix them first", entry.file.display()))
        })?;

        // Which definition in the file it is: the n-th with its qualified
        // name, as the history numbers namesakes.
        let qualified = work.symbols.qualified_name(id).unwrap_or_else(|| entry.name.clone());
        let nth =
            work.symbols.find(&entry.file, &qualified).iter().position(|other| *other == id).unwrap_or(0);
        let range = (0..parsed.symbols.len())
            .filter(|&i| parsed.qualified_name(i) == qualified)
            .nth(nth)
            .map(|i| parsed.symbols[i].range.clone())
            .ok_or_else(|| Error::Other(format!("`{qualified}` is not where the history says it is")))?;

        let inside = &content[range.clone()];
        let hits = occurrences(inside, old.as_bytes());
        if hits.is_empty() {
            return Err(Error::Other(format!("`{old}` does not appear in its own definition")));
        }
        let mut renamed = content[..range.start].to_vec();
        let mut at = 0;
        for hit in hits {
            renamed.extend_from_slice(&inside[at..hit]);
            renamed.extend_from_slice(new_name.as_bytes());
            at = hit + old.len();
        }
        renamed.extend_from_slice(&inside[at..]);
        renamed.extend_from_slice(&content[range.end..]);

        // Where else the old name is still written.
        let mut possible_references = Vec::new();
        let mut note = |path: &Path, text: &[u8], skip: Option<&std::ops::Range<usize>>| {
            for hit in occurrences(text, old.as_bytes()) {
                if skip.is_some_and(|range| range.contains(&hit))
                    || possible_references.len() >= MAX_REFERENCES
                {
                    continue;
                }
                let line = text[..hit].iter().filter(|&&c| c == b'\n').count() + 1;
                possible_references.push(format!("{}:{line}", path.display()));
            }
        };
        note(&entry.file, &content, Some(&range));
        let others: Vec<(PathBuf, TreeEntry)> = work
            .tree
            .iter()
            .filter(|(path, _)| *path != entry.file)
            .map(|(p, e)| (p.to_owned(), *e))
            .collect();
        for (path, other) in others {
            if dialect_for_path(&path).is_some() {
                if let Some(text) = self.store.get_blob(&other.blob)? {
                    note(&path, &text, None);
                }
            }
        }

        // The file, then the capture, told which symbol this is.
        let root = work.root.clone();
        let blob = self.store.put_blob(&renamed)?;
        self.write_file_in(&root, &entry.file, &TreeEntry { blob, mode: tree_entry.mode })?;
        let mut identities = Identities::new();
        identities.insert(&entry.file, range.start, id);
        let file = entry.file.clone();
        let ops = self.with(lane, |repo, work| {
            let intent = work.intent;
            repo.capture_work(work, [file], intent, &identities, crate::repo::Unsettled::Trust)
        })?;
        Ok(RenameResult {
            symbol: id.to_string(),
            from: entry.name.clone(),
            to: format!("{scope}{new_name}"),
            file: entry.file.display().to_string(),
            ops: ops.iter().map(|op| op.id.to_string()).collect(),
            possible_references,
        })
    }
}
