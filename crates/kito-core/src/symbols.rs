//! State of the symbols: which symbol has which ID, name and file.
//!
//! Like [`crate::Tree`], it is obtained by replaying the operations of a lane
//! in order, and depends only on what the ops declare. Hashes and
//! fingerprints are deliberately not part of it: they are recomputed from the
//! file contents whenever two versions are compared, so a newer grammar never
//! invalidates the identity of a symbol.

use crate::ids::{LaneId, SymbolId};
use crate::model::{Lang, OpKind, SymbolKind};
use imbl::ordmap::DiffItem;
use imbl::OrdMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// What the history says about one symbol at a given point.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub kind: SymbolKind,
    pub lang: Lang,
    pub name: String,
    /// Enclosing symbol (class → method).
    pub parent: Option<SymbolId>,
    pub file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SymbolError {
    #[error("symbol {0} already exists")]
    Exists(SymbolId),
    #[error("symbol {0} does not exist")]
    Missing(SymbolId),
    #[error("symbol {0} cannot be moved inside itself")]
    Cycle(SymbolId),
}

/// The symbols of a lane at a given point in the history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolTable {
    // Persistent maps, like the tree's: a copy of the table shares
    // everything with the original and costs nothing.
    symbols: OrdMap<SymbolId, SymbolEntry>,
    by_file: OrdMap<PathBuf, BTreeSet<SymbolId>>,
    /// Per lane of origin, the next free counter for a new [`SymbolId`].
    next_seq: BTreeMap<LaneId, u64>,
}

impl SymbolTable {
    pub fn new() -> SymbolTable {
        SymbolTable::default()
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    pub fn get(&self, id: SymbolId) -> Option<&SymbolEntry> {
        self.symbols.get(&id)
    }

    /// Every symbol, in order of ID.
    pub fn iter(&self) -> impl Iterator<Item = (SymbolId, &SymbolEntry)> {
        self.symbols.iter().map(|(id, entry)| (*id, entry))
    }

    /// The symbols of a file, in order of ID.
    pub fn in_file<'a>(&'a self, file: &Path) -> impl Iterator<Item = (SymbolId, &'a SymbolEntry)> {
        self.by_file
            .get(file)
            .into_iter()
            .flatten()
            .filter_map(|id| self.symbols.get(id).map(|entry| (*id, entry)))
    }

    /// The symbols this table and `other` do not have in the same way: one
    /// has it and the other does not, or what is known of it differs. Like
    /// [`crate::Tree::differences`], what the two tables share because one
    /// was copied from the other is not looked at.
    pub fn changed<'a>(&'a self, other: &'a SymbolTable) -> impl Iterator<Item = SymbolId> + 'a {
        self.symbols.diff(&other.symbols).map(|item| match item {
            DiffItem::Add(id, _) | DiffItem::Remove(id, _) => *id,
            DiffItem::Update { old: (id, _), .. } => *id,
        })
    }

    /// The counter a lane will use for its next new symbol. Never reused:
    /// it only grows, even when symbols are removed.
    pub fn next_seq(&self, lane: LaneId) -> u64 {
        self.next_seq.get(&lane).copied().unwrap_or(0)
    }

    /// Makes sure the next symbol `lane` creates gets a counter of at least
    /// `seq`: for a table rebuilt from a point that has not seen every ID
    /// the lane already used.
    pub fn reserve(&mut self, lane: LaneId, seq: u64) {
        if seq > 0 {
            let next = self.next_seq.entry(lane).or_insert(0);
            *next = (*next).max(seq);
        }
    }

    /// Name of a symbol qualified by its enclosing symbols: `Server.start`.
    pub fn qualified_name(&self, id: SymbolId) -> Option<String> {
        let mut parts = Vec::new();
        let mut cursor = Some(id);
        // A cycle cannot be built through the ops, but a bound costs nothing.
        for _ in 0..64 {
            let Some(current) = cursor else { break };
            let entry = self.symbols.get(&current)?;
            parts.push(entry.name.as_str());
            cursor = entry.parent;
        }
        parts.reverse();
        Some(parts.join("."))
    }

    /// The symbols of `file` whose qualified name, or plain name, is `name`.
    pub fn find(&self, file: &Path, name: &str) -> Vec<SymbolId> {
        let qualified: Vec<SymbolId> = self
            .in_file(file)
            .filter(|(id, _)| self.qualified_name(*id).as_deref() == Some(name))
            .map(|(id, _)| id)
            .collect();
        if !qualified.is_empty() {
            return qualified;
        }
        self.in_file(file).filter(|(_, entry)| entry.name == name).map(|(id, _)| id).collect()
    }

    /// `true` if `id` is `container` or is nested in it at any depth.
    fn is_inside(&self, id: SymbolId, container: SymbolId) -> bool {
        let mut cursor = Some(id);
        for _ in 0..64 {
            match cursor {
                Some(current) if current == container => return true,
                Some(current) => cursor = self.symbols.get(&current).and_then(|entry| entry.parent),
                None => return false,
            }
        }
        false
    }

    fn unlink(&mut self, file: &Path, id: SymbolId) {
        if let Some(ids) = self.by_file.get_mut(file) {
            ids.remove(&id);
            if ids.is_empty() {
                self.by_file.remove(file);
            }
        }
    }

    /// Applies an operation. Ops that do not concern symbols leave the table
    /// unchanged. If the operation cannot be applied nothing changes.
    pub fn apply(&mut self, kind: &OpKind) -> Result<(), SymbolError> {
        match kind {
            OpKind::AddSymbol { sym, .. } => {
                if self.symbols.contains_key(&sym.id) {
                    return Err(SymbolError::Exists(sym.id));
                }
                let entry = SymbolEntry {
                    kind: sym.kind,
                    lang: sym.lang,
                    name: sym.name.clone(),
                    parent: sym.parent,
                    file: sym.file.clone(),
                };
                self.by_file.entry(entry.file.clone()).or_default().insert(sym.id);
                self.symbols.insert(sym.id, entry);
                let next = self.next_seq.entry(sym.id.origin()).or_insert(0);
                *next = (*next).max(sym.id.seq() + 1);
            }
            OpKind::RemoveSymbol { id } => {
                let entry = self.symbols.remove(id).ok_or(SymbolError::Missing(*id))?;
                self.unlink(&entry.file, *id);
            }
            OpKind::Rename { id, to, .. } => {
                self.symbols.get_mut(id).ok_or(SymbolError::Missing(*id))?.name = to.clone();
            }
            // A moved container takes everything it contains along.
            OpKind::Move { id, to, parent, .. } => {
                let from = self.symbols.get(id).ok_or(SymbolError::Missing(*id))?.file.clone();
                if let Some(parent) = parent {
                    if !self.symbols.contains_key(parent) {
                        return Err(SymbolError::Missing(*parent));
                    }
                    if self.is_inside(*parent, *id) {
                        return Err(SymbolError::Cycle(*id));
                    }
                }
                let moving: Vec<SymbolId> = self
                    .by_file
                    .get(&from)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|candidate| self.is_inside(*candidate, *id))
                    .collect();
                for moved in moving {
                    if let Some(entry) = self.symbols.get_mut(&moved) {
                        entry.file = to.file.clone();
                    }
                    self.unlink(&from, moved);
                    self.by_file.entry(to.file.clone()).or_default().insert(moved);
                }
                if let Some(entry) = self.symbols.get_mut(id) {
                    entry.parent = *parent;
                }
            }
            OpKind::ModifyBody { id, .. } | OpKind::ModifySignature { id, .. } => {
                if !self.symbols.contains_key(id) {
                    return Err(SymbolError::Missing(*id));
                }
            }
            // The symbols of a renamed file follow it.
            OpKind::FileRename { from, to } => {
                if let Some(ids) = self.by_file.remove(from) {
                    for id in &ids {
                        if let Some(entry) = self.symbols.get_mut(id) {
                            entry.file = to.clone();
                        }
                    }
                    self.by_file.entry(to.clone()).or_default().extend(ids);
                }
            }
            // Removing a file says nothing about its symbols: each of them is
            // removed, or moved elsewhere, by an op of its own.
            OpKind::FileRemove { .. } | OpKind::FileAdd { .. } | OpKind::TextEdit { .. } => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::ids::BlobId;
    use crate::model::{Fingerprint, Location, Symbol};

    fn add(seq: u64, name: &str, parent: Option<u64>, file: &str) -> OpKind {
        let lane = LaneId(3);
        OpKind::AddSymbol {
            sym: Symbol {
                id: SymbolId::new(lane, seq),
                kind: if parent.is_some() { SymbolKind::Method } else { SymbolKind::Class },
                lang: Lang::TypeScript,
                name: name.to_owned(),
                parent: parent.map(|p| SymbolId::new(lane, p)),
                file: file.into(),
                signature: Hash::ZERO,
                body: Hash::ZERO,
                fingerprint: Fingerprint::default(),
            },
            text: BlobId::of(name.as_bytes()),
        }
    }

    fn id(seq: u64) -> SymbolId {
        SymbolId::new(LaneId(3), seq)
    }

    fn at(file: &str) -> Location {
        Location { file: file.into(), line: 1, start: 0, end: 1 }
    }

    #[test]
    fn replaying_ops_gives_the_table() {
        let mut t = SymbolTable::new();
        t.apply(&add(0, "Server", None, "a.ts")).unwrap();
        t.apply(&add(1, "start", Some(0), "a.ts")).unwrap();
        t.apply(&add(2, "Other", None, "b.ts")).unwrap();
        assert_eq!(t.qualified_name(id(1)).as_deref(), Some("Server.start"));
        assert_eq!(t.find(Path::new("a.ts"), "Server.start"), [id(1)]);
        assert_eq!(t.find(Path::new("a.ts"), "start"), [id(1)]);
        assert!(t.find(Path::new("b.ts"), "start").is_empty());

        t.apply(&OpKind::Rename { id: id(1), from: "start".into(), to: "run".into() }).unwrap();
        t.apply(&OpKind::Move { id: id(2), from: at("b.ts"), to: at("a.ts"), parent: None }).unwrap();
        // A container moves with its members.
        t.apply(&OpKind::Move { id: id(0), from: at("a.ts"), to: at("z.ts"), parent: None }).unwrap();
        assert_eq!(t.in_file(Path::new("z.ts")).map(|(i, _)| i.seq()).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(t.in_file(Path::new("a.ts")).map(|(i, _)| i.seq()).collect::<Vec<_>>(), [2]);
        t.apply(&OpKind::Move { id: id(0), from: at("z.ts"), to: at("a.ts"), parent: None }).unwrap();
        assert_eq!(
            t.in_file(Path::new("a.ts")).map(|(i, e)| (i.seq(), e.name.as_str())).collect::<Vec<_>>(),
            [(0, "Server"), (1, "run"), (2, "Other")]
        );
        assert_eq!(t.in_file(Path::new("b.ts")).count(), 0);

        // A renamed file takes its symbols along.
        t.apply(&OpKind::FileRename { from: "a.ts".into(), to: "src/c.ts".into() }).unwrap();
        assert_eq!(t.get(id(0)).unwrap().file, PathBuf::from("src/c.ts"));
        t.apply(&OpKind::RemoveSymbol { id: id(2) }).unwrap();
        assert_eq!(t.len(), 2);
        // Removing the file does not remove them: that takes their own ops.
        t.apply(&OpKind::FileRemove { path: "src/c.ts".into() }).unwrap();
        assert_eq!(t.len(), 2);
        t.apply(&OpKind::RemoveSymbol { id: id(1) }).unwrap();
        t.apply(&OpKind::RemoveSymbol { id: id(0) }).unwrap();
        assert!(t.is_empty());
        // IDs are never reused.
        assert_eq!(t.next_seq(LaneId(3)), 3);
        assert_eq!(t.next_seq(LaneId(9)), 0);
        // Two tables differ in what happened to one and not to the other.
        let mut other = t.clone();
        assert_eq!(t.changed(&other).count(), 0);
        other.apply(&add(20, "Extra", None, "a.ts")).unwrap();
        assert_eq!(t.changed(&other).collect::<Vec<_>>(), [id(20)]);
        // Nor are the ones a lane used where this table could not see them.
        t.reserve(LaneId(3), 2);
        assert_eq!(t.next_seq(LaneId(3)), 3);
        t.reserve(LaneId(3), 10);
        t.apply(&add(10, "Later", None, "a.ts")).unwrap();
        assert_eq!(t.next_seq(LaneId(3)), 11);
    }

    #[test]
    fn a_move_can_change_the_enclosing_symbol() {
        let mut t = SymbolTable::new();
        t.apply(&add(0, "Server", None, "a.ts")).unwrap();
        t.apply(&add(1, "start", Some(0), "a.ts")).unwrap();
        t.apply(&add(2, "Client", None, "b.ts")).unwrap();

        // A method goes to another class, in another file.
        t.apply(&OpKind::Move { id: id(1), from: at("a.ts"), to: at("b.ts"), parent: Some(id(2)) }).unwrap();
        assert_eq!(t.qualified_name(id(1)).as_deref(), Some("Client.start"));
        assert_eq!(t.in_file(Path::new("b.ts")).map(|(i, _)| i.seq()).collect::<Vec<_>>(), [1, 2]);
        // The class it left moves on its own from now on.
        t.apply(&OpKind::Move { id: id(0), from: at("a.ts"), to: at("z.ts"), parent: None }).unwrap();
        assert_eq!(t.get(id(1)).unwrap().file, PathBuf::from("b.ts"));

        // To the top level, staying in the same file.
        t.apply(&OpKind::Move { id: id(1), from: at("b.ts"), to: at("b.ts"), parent: None }).unwrap();
        assert_eq!(t.qualified_name(id(1)).as_deref(), Some("start"));
        assert_eq!(t.in_file(Path::new("b.ts")).count(), 2);

        // A symbol cannot end up inside itself, nor inside one that does not exist.
        let before = t.clone();
        t.apply(&OpKind::Move { id: id(1), from: at("b.ts"), to: at("b.ts"), parent: Some(id(2)) }).unwrap();
        assert_eq!(
            t.apply(&OpKind::Move { id: id(2), from: at("b.ts"), to: at("b.ts"), parent: Some(id(1)) }),
            Err(SymbolError::Cycle(id(2)))
        );
        assert_eq!(
            t.apply(&OpKind::Move { id: id(2), from: at("b.ts"), to: at("c.ts"), parent: Some(id(9)) }),
            Err(SymbolError::Missing(id(9)))
        );
        assert_eq!(t.get(id(2)), before.get(id(2)));
    }

    #[test]
    fn inapplicable_ops_are_rejected_without_side_effects() {
        let mut t = SymbolTable::new();
        t.apply(&add(0, "Server", None, "a.ts")).unwrap();
        let before = t.clone();
        assert_eq!(t.apply(&add(0, "Again", None, "a.ts")), Err(SymbolError::Exists(id(0))));
        for op in [
            OpKind::RemoveSymbol { id: id(7) },
            OpKind::Rename { id: id(7), from: "a".into(), to: "b".into() },
            OpKind::Move { id: id(7), from: at("a.ts"), to: at("b.ts"), parent: None },
            OpKind::ModifySignature { id: id(7), before: BlobId::of(b"a"), after: BlobId::of(b"b") },
        ] {
            assert_eq!(t.apply(&op), Err(SymbolError::Missing(id(7))));
        }
        assert_eq!(t, before);
    }
}
