//! AST differ: matches the symbols of two versions, keeping their IDs
//! stable, and produces the symbolic ops.
//!
//! The old side is what the history knows ([`SymbolTable`]); the new side is
//! what the parser sees now. Old symbols are matched to new ones in four
//! passes, and the first pass that finds a match wins:
//!
//! 1. same name, same kind and same parent → same ID;
//! 2. same body hash, different name → `Rename`;
//! 3. same signature and a similar fingerprint, or a very similar
//!    fingerprint → same ID, `ModifyBody` (and `Rename` if the name differs);
//! 4. no match → `AddSymbol`; old symbols left without a match → `RemoveSymbol`.
//!
//! Moves between files are detected the same way, by pairing the removed
//! and the added symbols of the same batch.
//!
//! The thresholds are conservative on purpose: a false rename pollutes both
//! the history and the merges, a missed one only costs an add and a remove.

use crate::fileops::inline_edits;
use kito_core::{BlobId, LaneId, Location, OpKind, Symbol, SymbolEntry, SymbolId, SymbolTable};
use kito_parse::{ParsedFile, ParsedSymbol};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A body with fewer tokens than this is too common to identify a symbol
/// (`{}`, `pass`, `return x;`).
pub const MIN_BODY_TOKENS: usize = 8;
/// A definition with fewer tokens than this has no meaningful fingerprint.
pub const MIN_SHAPE_TOKENS: usize = 12;
/// Fingerprint similarity above which two definitions are the same symbol.
pub const SIMILARITY: f32 = 0.8;
/// The same, when the two definitions also have the same signature.
pub const SIMILARITY_SAME_SIGNATURE: f32 = 0.5;
/// Share of member names two containers must have in common to be the same
/// symbol when their own text is too small to tell.
pub const MEMBER_OVERLAP: f32 = 0.8;

/// One version of a file: its content and its symbols.
#[derive(Clone, Copy)]
pub struct Version<'a> {
    pub content: &'a [u8],
    pub parsed: &'a ParsedFile,
}

/// A file whose content changed. `old: None` means the file is new or its
/// old content could not be parsed; `new: None` means the file is gone.
#[derive(Clone, Copy)]
pub struct FileChange<'a> {
    pub path: &'a Path,
    pub old: Option<Version<'a>>,
    pub new: Option<Version<'a>>,
}

/// The symbolic ops of a batch and the blobs they refer to.
#[derive(Debug, Default)]
pub struct SymbolOps {
    /// In an order that applies to the [`SymbolTable`]: renames, moves and
    /// modifications, then removals (members first), then additions
    /// (containers first).
    pub ops: Vec<OpKind>,
    /// Texts the ops cite by [`BlobId`]: to be stored before the ops.
    pub blobs: Vec<Vec<u8>>,
}

/// A symbol the history knows, with what the old content says about it (if
/// the old content could be parsed and still contains it).
struct Old<'a> {
    id: SymbolId,
    entry: &'a SymbolEntry,
    seen: Option<(&'a ParsedSymbol, &'a [u8])>,
    children: Vec<usize>,
}

/// A symbol of the new content.
struct New<'a> {
    file: &'a Path,
    parsed: &'a ParsedFile,
    sym: &'a ParsedSymbol,
    content: &'a [u8],
    parent: Option<usize>,
    children: Vec<usize>,
}

struct Differ<'a> {
    olds: Vec<Old<'a>>,
    news: Vec<New<'a>>,
    /// ID given to each new symbol: the matched old one, or a fresh one.
    assigned: Vec<Option<SymbolId>>,
    leftover_old: Vec<usize>,
    leftover_new: Vec<usize>,
    changes: Vec<OpKind>,
    blobs: Vec<Vec<u8>>,
    stored: HashSet<BlobId>,
}

/// Compares the old and the new version of the changed files and returns
/// the symbolic ops that lead from one to the other. New symbols get IDs
/// from `lane`'s counter.
pub fn diff_symbols(table: &SymbolTable, lane: LaneId, changes: &[FileChange<'_>]) -> SymbolOps {
    let mut differ = Differ {
        olds: Vec::new(),
        news: Vec::new(),
        assigned: Vec::new(),
        leftover_old: Vec::new(),
        leftover_new: Vec::new(),
        changes: Vec::new(),
        blobs: Vec::new(),
        stored: HashSet::new(),
    };

    // Old and new symbols of every file, and the top-level ones of each.
    let mut scopes: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
    for change in changes {
        let old_roots = differ.load_old(table, change);
        let new_roots = differ.load_new(change);
        scopes.push((old_roots, new_roots));
    }
    for (old_roots, new_roots) in scopes {
        differ.match_scope(old_roots, new_roots, false);
    }
    differ.match_moves();

    // Whatever is still unmatched is removed or added, with all it contains.
    let mut ops = std::mem::take(&mut differ.changes);
    for root in std::mem::take(&mut differ.leftover_old) {
        differ.remove(root, &mut ops);
    }
    let mut next = table.next_seq(lane);
    for root in std::mem::take(&mut differ.leftover_new) {
        differ.add(root, lane, &mut next, &mut ops);
    }
    SymbolOps { ops, blobs: differ.blobs }
}

fn text<'a>(content: &'a [u8], range: &std::ops::Range<usize>) -> &'a [u8] {
    content.get(range.clone()).unwrap_or_default()
}

fn location(file: &Path, sym: &ParsedSymbol) -> Location {
    Location {
        file: file.to_owned(),
        line: sym.line,
        start: sym.range.start as u32,
        end: sym.range.end as u32,
    }
}

impl<'a> Differ<'a> {
    /// Loads what the history knows about the file, and recognizes those
    /// symbols in the old content: same parent, kind and name, in order.
    /// Returns the top-level ones.
    fn load_old(&mut self, table: &'a SymbolTable, change: &FileChange<'a>) -> Vec<usize> {
        let entries: Vec<(SymbolId, &SymbolEntry)> = table.in_file(change.path).collect();
        let mut seen: HashMap<SymbolId, (&ParsedSymbol, &[u8])> = HashMap::new();
        if let Some(old) = change.old {
            let mut ids: Vec<Option<SymbolId>> = vec![None; old.parsed.symbols.len()];
            for (i, sym) in old.parsed.symbols.iter().enumerate() {
                let parent = match sym.parent {
                    None => None,
                    Some(p) => match ids[p] {
                        Some(id) => Some(id),
                        None => continue, // the parent is unknown to the history, so is this
                    },
                };
                let known = entries.iter().find(|(id, entry)| {
                    !seen.contains_key(id)
                        && entry.parent == parent
                        && entry.kind == sym.kind
                        && entry.name == sym.name
                });
                if let Some((id, _)) = known {
                    seen.insert(*id, (sym, old.content));
                    ids[i] = Some(*id);
                }
            }
        }
        let base = self.olds.len();
        let index: HashMap<SymbolId, usize> =
            entries.iter().enumerate().map(|(i, (id, _))| (*id, base + i)).collect();
        for (id, entry) in &entries {
            self.olds.push(Old { id: *id, entry, seen: seen.get(id).copied(), children: Vec::new() });
        }
        let mut roots = Vec::new();
        for (i, (_, entry)) in entries.iter().enumerate() {
            match entry.parent.and_then(|p| index.get(&p)) {
                Some(&parent) => self.olds[parent].children.push(base + i),
                None => roots.push(base + i),
            }
        }
        roots
    }

    /// Loads the symbols of the new content. Returns the top-level ones.
    fn load_new(&mut self, change: &FileChange<'a>) -> Vec<usize> {
        let Some(new) = change.new else { return Vec::new() };
        let base = self.news.len();
        let mut roots = Vec::new();
        for (i, sym) in new.parsed.symbols.iter().enumerate() {
            let parent = sym.parent.map(|p| base + p);
            self.news.push(New {
                file: change.path,
                parsed: new.parsed,
                sym,
                content: new.content,
                parent,
                children: Vec::new(),
            });
            self.assigned.push(None);
            match parent {
                Some(p) => self.news[p].children.push(base + i),
                None => roots.push(base + i),
            }
        }
        roots
    }

    /// Names of the members two containers have in common, as a share of the
    /// larger of the two.
    fn member_overlap(&self, old: usize, new: usize) -> f32 {
        let old_names: Vec<&str> =
            self.olds[old].children.iter().map(|&c| self.olds[c].entry.name.as_str()).collect();
        let new_names: HashSet<&str> =
            self.news[new].children.iter().map(|&c| self.news[c].sym.name.as_str()).collect();
        let common = old_names.iter().filter(|name| new_names.contains(*name)).count();
        if common < 2 {
            return 0.0;
        }
        common as f32 / old_names.len().max(new_names.len()) as f32
    }

    /// How confident we are that an old and a new symbol with different
    /// names are the same one (pass 3). `None`: not the same.
    fn resemblance(&self, old: usize, new: usize) -> Option<f32> {
        let (o, n) = (&self.olds[old], self.news[new].sym);
        let (seen, _) = o.seen?;
        if o.entry.kind != n.kind {
            return None;
        }
        if seen.tokens >= MIN_SHAPE_TOKENS && n.tokens >= MIN_SHAPE_TOKENS {
            let similarity = seen.fingerprint.similarity(&n.fingerprint);
            let threshold =
                if seen.signature == n.signature { SIMILARITY_SAME_SIGNATURE } else { SIMILARITY };
            return (similarity >= threshold).then_some(similarity);
        }
        // Too little text of its own (a class that is only methods): judge by
        // its members.
        let overlap = self.member_overlap(old, new);
        (overlap >= MEMBER_OVERLAP).then_some(overlap)
    }

    /// Matches the symbols of one scope (same parent), then goes down into
    /// each matched pair. `moved`: the scope as a whole changed file, along
    /// with its container.
    fn match_scope(&mut self, olds: Vec<usize>, news: Vec<usize>, moved: bool) {
        let mut free_old = olds;
        let mut free_new = news;
        let mut pairs: Vec<(usize, usize)> = Vec::new();

        // Pass 1: same name and kind. Among namesakes (overloads) the ones
        // that did not change pair up first, then the ones that changed only
        // in part, then the rest in order: adding an overload in front must
        // not shift the identity of the others.
        for exactness in [2, 1, 0] {
            free_new.retain(|&n| {
                let sym = self.news[n].sym;
                let hit = free_old.iter().position(|&o| {
                    let old = &self.olds[o];
                    if old.entry.kind != sym.kind || old.entry.name != sym.name {
                        return false;
                    }
                    let unchanged = old.seen.map_or(0, |(seen, _)| {
                        usize::from(seen.signature == sym.signature) + usize::from(seen.body == sym.body)
                    });
                    unchanged >= exactness
                });
                match hit {
                    Some(at) => {
                        pairs.push((free_old.remove(at), n));
                        false
                    }
                    None => true,
                }
            });
        }

        // Pass 2: same body, different name. Only when the body is big enough
        // to be telling and the pairing is unambiguous.
        free_new.retain(|&n| {
            let sym = self.news[n].sym;
            if sym.body_tokens < MIN_BODY_TOKENS {
                return true;
            }
            let same_body = |o: &usize| {
                let old = &self.olds[*o];
                old.entry.kind == sym.kind && old.seen.is_some_and(|(seen, _)| seen.body == sym.body)
            };
            let mut candidates = free_old.iter().filter(|o| same_body(o));
            match (candidates.next().copied(), candidates.next()) {
                (Some(o), None) => {
                    free_old.retain(|&x| x != o);
                    pairs.push((o, n));
                    false
                }
                _ => true,
            }
        });

        // Pass 3: resemblance, best pairs first.
        let mut candidates: Vec<(f32, usize, usize)> = Vec::new();
        for &o in &free_old {
            for &n in &free_new {
                if let Some(score) = self.resemblance(o, n) {
                    candidates.push((score, o, n));
                }
            }
        }
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        for (_, o, n) in candidates {
            if free_old.contains(&o) && free_new.contains(&n) {
                free_old.retain(|&x| x != o);
                free_new.retain(|&x| x != n);
                pairs.push((o, n));
            }
        }

        // Pass 4 happens at the end, for whatever no scope and no move claimed.
        self.leftover_old.extend(free_old);
        self.leftover_new.extend(free_new);

        pairs.sort_by_key(|&(_, n)| n);
        for (o, n) in pairs {
            self.pair(o, n, moved);
        }
    }

    /// Records that an old and a new symbol are the same one, emits what
    /// changed between them, and matches their members. `moved_along`: the
    /// symbol changed file together with its container, whose `Move` already
    /// says so.
    fn pair(&mut self, old: usize, new: usize, moved_along: bool) {
        let id = self.olds[old].id;
        self.assigned[new] = Some(id);
        let (entry, seen) = (self.olds[old].entry, self.olds[old].seen);
        let (file, sym, content) = (self.news[new].file, self.news[new].sym, self.news[new].content);

        if entry.name != sym.name {
            self.changes.push(OpKind::Rename { id, from: entry.name.clone(), to: sym.name.clone() });
        }
        let moved = entry.file != file;
        if moved && !moved_along {
            let from = match seen {
                Some((seen, _)) => location(&entry.file, seen),
                None => Location { file: entry.file.clone(), line: 0, start: 0, end: 0 },
            };
            self.changes.push(OpKind::Move { id, from, to: location(file, sym) });
        }
        // Without the old content there is nothing to compare: the change of
        // the file as a whole is recorded anyway by its `TextEdit`.
        if let Some((seen, old_content)) = seen {
            if seen.signature != sym.signature {
                let before = self.blob(text(old_content, &seen.header));
                let after = self.blob(text(content, &sym.header));
                self.changes.push(OpKind::ModifySignature { id, before, after });
            }
            if seen.body != sym.body {
                let (old_text, new_text) = (text(old_content, &seen.range), text(content, &sym.range));
                let edits = inline_edits(old_text, new_text);
                let (before, after) = (self.blob(old_text), self.blob(new_text));
                self.changes.push(OpKind::ModifyBody { id, before, after, edits });
            }
        }

        let old_members = self.olds[old].children.clone();
        let new_members = self.news[new].children.clone();
        if !old_members.is_empty() || !new_members.is_empty() {
            self.match_scope(old_members, new_members, moved);
        }
    }

    /// Moves between files: a top-level symbol that disappeared from one
    /// file and one that appeared in another, with the same body, or with
    /// the same name and a very similar shape.
    fn match_moves(&mut self) {
        let mut candidates: Vec<(f32, usize, usize)> = Vec::new();
        for &o in &self.leftover_old {
            let old = &self.olds[o];
            let Some((seen, _)) = old.seen else { continue };
            if old.entry.parent.is_some() {
                continue;
            }
            for &n in &self.leftover_new {
                let new = &self.news[n];
                if new.parent.is_some() || new.file == old.entry.file || new.sym.kind != old.entry.kind {
                    continue;
                }
                let same_name = old.entry.name == new.sym.name;
                let score = if seen.body == new.sym.body && new.sym.body_tokens >= MIN_BODY_TOKENS {
                    Some(1.0)
                } else if same_name && seen.tokens >= MIN_SHAPE_TOKENS && new.sym.tokens >= MIN_SHAPE_TOKENS {
                    let similarity = seen.fingerprint.similarity(&new.sym.fingerprint);
                    (similarity >= SIMILARITY).then_some(similarity)
                } else if same_name {
                    let overlap = self.member_overlap(o, n);
                    (overlap >= MEMBER_OVERLAP).then_some(overlap)
                } else {
                    None
                };
                if let Some(score) = score {
                    // At equal evidence, the one that kept its name.
                    candidates.push((score + if same_name { 0.5 } else { 0.0 }, o, n));
                }
            }
        }
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        for (_, o, n) in candidates {
            if self.leftover_old.contains(&o) && self.leftover_new.contains(&n) {
                self.leftover_old.retain(|&x| x != o);
                self.leftover_new.retain(|&x| x != n);
                self.pair(o, n, false);
            }
        }
    }

    fn blob(&mut self, content: &[u8]) -> BlobId {
        let id = BlobId::of(content);
        if self.stored.insert(id) {
            self.blobs.push(content.to_vec());
        }
        id
    }

    /// `RemoveSymbol` for a symbol and everything it contains, members first.
    fn remove(&self, old: usize, ops: &mut Vec<OpKind>) {
        for &member in &self.olds[old].children {
            self.remove(member, ops);
        }
        ops.push(OpKind::RemoveSymbol { id: self.olds[old].id });
    }

    /// `AddSymbol` for a symbol and everything it contains, container first.
    fn add(&mut self, new: usize, lane: LaneId, next: &mut u64, ops: &mut Vec<OpKind>) {
        let id = SymbolId::new(lane, *next);
        *next += 1;
        self.assigned[new] = Some(id);
        let (file, sym, content, lang) =
            (self.news[new].file, self.news[new].sym, self.news[new].content, self.news[new].parsed.lang);
        let parent = self.news[new].parent.and_then(|p| self.assigned[p]);
        let text = self.blob(text(content, &sym.range));
        ops.push(OpKind::AddSymbol {
            sym: Symbol {
                id,
                kind: sym.kind,
                lang,
                name: sym.name.clone(),
                parent,
                file: file.to_owned(),
                signature: sym.signature,
                body: sym.body,
                fingerprint: sym.fingerprint.clone(),
            },
            text,
        });
        for member in self.news[new].children.clone() {
            self.add(member, lane, next, ops);
        }
    }
}
