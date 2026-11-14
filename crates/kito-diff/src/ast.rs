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
//! Moves are detected the same way, by pairing the removed and the added
//! symbols of the same batch: a symbol that changed file, or that changed
//! enclosing symbol (a class that left its namespace, a method that went to
//! another class).
//!
//! The thresholds are conservative on purpose: a false rename pollutes both
//! the history and the merges, a missed one only costs an add and a remove.

use crate::fileops::inline_edits;
use kito_core::{BlobId, LaneId, Location, OpKind, Symbol, SymbolEntry, SymbolId, SymbolTable};
use kito_parse::{ParsedFile, ParsedSymbol};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

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
    /// In an order that applies to the [`SymbolTable`]: additions
    /// (containers first), then moves (outermost destination first), renames
    /// and modifications, then removals (members first).
    pub ops: Vec<OpKind>,
    /// Texts the ops cite by [`BlobId`]: to be stored before the ops.
    pub blobs: Vec<Vec<u8>>,
}

/// Identities known in advance, which no heuristic may contradict: the
/// symbol that starts at a given byte of a file's new content is the symbol
/// with a given ID. A merge knows which symbol each piece of its result
/// came from; an explicit rename knows what it renamed.
#[derive(Clone, Debug, Default)]
pub struct Identities {
    by_start: HashMap<(PathBuf, usize), SymbolId>,
    first_new: u64,
    /// Symbols that were removed and may come back: by where they were and
    /// what they were called.
    gone: HashMap<Place, VecDeque<SymbolId>>,
}

/// File, enclosing symbol, kind and name: what a symbol that comes back is
/// recognized by.
type Place = (PathBuf, Option<SymbolId>, kito_core::SymbolKind, String);

impl Identities {
    pub fn new() -> Identities {
        Identities::default()
    }

    /// The symbol whose definition starts at byte `start` of `file` is `id`.
    pub fn insert(&mut self, file: &Path, start: usize, id: SymbolId) {
        self.by_start.insert((file.to_owned(), start), id);
    }

    pub fn is_empty(&self) -> bool {
        self.by_start.is_empty()
    }

    /// `true` if no symbol is known in advance and none may be coming back:
    /// the symbols of new files are then simply new.
    pub fn knows_nothing(&self) -> bool {
        self.by_start.is_empty() && self.gone.is_empty()
    }

    /// A symbol with this ID was removed from `file`, where it had this
    /// enclosing symbol, kind and name. If a new symbol shows up in the
    /// same place with the same name, it is that symbol coming back, and
    /// gets its ID again: what is removed by one save and restored by a
    /// later one (a rollback, a cut and paste in two steps, a file caught
    /// while it was being rewritten) keeps its history.
    pub fn gone(
        &mut self,
        file: &Path,
        parent: Option<SymbolId>,
        kind: kito_core::SymbolKind,
        name: &str,
        id: SymbolId,
    ) {
        self.gone.entry((file.to_owned(), parent, kind, name.to_owned())).or_default().push_back(id);
    }

    /// The counter of the IDs given to new symbols starts at least here.
    /// For a lane whose state is being rebuilt on another base: the table
    /// it is compared with has not seen the IDs the lane already used.
    pub fn start_new_ids_at(&mut self, seq: u64) {
        self.first_new = self.first_new.max(seq);
    }

    fn get(&self, file: &Path, start: usize) -> Option<SymbolId> {
        self.by_start.get(&(file.to_owned(), start)).copied()
    }
}

/// A symbol the history knows, with what the old content says about it (if
/// the old content could be parsed and still contains it).
struct Old<'a> {
    id: SymbolId,
    entry: &'a SymbolEntry,
    seen: Option<(&'a ParsedSymbol, &'a [u8])>,
    parent: Option<usize>,
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
    table: &'a SymbolTable,
    olds: Vec<Old<'a>>,
    news: Vec<New<'a>>,
    /// Where each loaded old symbol is in `olds`.
    old_at: HashMap<SymbolId, usize>,
    /// Per new symbol: the identity known in advance, if any.
    known: Vec<Option<SymbolId>>,
    /// Old symbols some new symbol is known to be: no heuristic may give
    /// them to another one.
    claimed: HashSet<SymbolId>,
    /// Removed symbols that may come back, and the IDs already given out.
    gone: HashMap<Place, VecDeque<SymbolId>>,
    given: HashSet<SymbolId>,
    /// Per old symbol: a new symbol was recognized as the same one.
    matched: Vec<bool>,
    /// ID given to each new symbol: the matched old one, or a fresh one.
    assigned: Vec<Option<SymbolId>>,
    /// Roots of the subtrees no scope matched (some are claimed by a move later).
    leftover_old: Vec<usize>,
    leftover_new: Vec<usize>,
    /// `Move` ops, each with the new symbol it leads to.
    moves: Vec<(usize, OpKind)>,
    changes: Vec<OpKind>,
    blobs: Vec<Vec<u8>>,
    stored: HashSet<BlobId>,
}

/// Which symbol of the history each symbol of a parsed file is: recognized
/// as the differ does, by enclosing symbol, kind and name, namesakes in
/// order. `None` for the ones the history does not know.
pub fn identify(table: &SymbolTable, path: &Path, parsed: &ParsedFile) -> Vec<Option<SymbolId>> {
    let mut known: HashMap<(Option<SymbolId>, kito_core::SymbolKind, &str), VecDeque<SymbolId>> =
        HashMap::new();
    for (id, entry) in table.in_file(path) {
        known.entry((entry.parent, entry.kind, entry.name.as_str())).or_default().push_back(id);
    }
    let mut ids: Vec<Option<SymbolId>> = Vec::with_capacity(parsed.symbols.len());
    for sym in &parsed.symbols {
        let parent = match sym.parent {
            None => Some(None),
            Some(p) => ids[p].map(Some),
        };
        let id = parent
            .and_then(|parent| known.get_mut(&(parent, sym.kind, sym.name.as_str())))
            .and_then(VecDeque::pop_front);
        ids.push(id);
    }
    ids
}

/// Compares the old and the new version of the changed files and returns
/// the symbolic ops that lead from one to the other. New symbols get IDs
/// from `lane`'s counter.
pub fn diff_symbols(table: &SymbolTable, lane: LaneId, changes: &[FileChange<'_>]) -> SymbolOps {
    diff_symbols_with(table, lane, changes, &Identities::default())
}

/// Like [`diff_symbols`], with some identities known in advance: those
/// symbols are paired as told (or added with the ID told, if the table does
/// not know it yet), and the heuristics only see the rest.
pub fn diff_symbols_with(
    table: &SymbolTable,
    lane: LaneId,
    changes: &[FileChange<'_>],
    identities: &Identities,
) -> SymbolOps {
    let mut differ = Differ {
        table,
        olds: Vec::new(),
        news: Vec::new(),
        old_at: HashMap::new(),
        known: Vec::new(),
        claimed: HashSet::new(),
        gone: identities.gone.clone(),
        given: HashSet::new(),
        matched: Vec::new(),
        assigned: Vec::new(),
        leftover_old: Vec::new(),
        leftover_new: Vec::new(),
        moves: Vec::new(),
        changes: Vec::new(),
        blobs: Vec::new(),
        stored: HashSet::new(),
    };

    // Old and new symbols of every file, and the top-level ones of each.
    let mut scopes: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
    for change in changes {
        let old_roots = differ.load_old(table, change);
        let new_roots = differ.load_new(change, identities);
        scopes.push((old_roots, new_roots));
    }
    differ.claimed =
        differ.known.iter().flatten().copied().filter(|id| differ.old_at.contains_key(id)).collect();
    for (old_roots, new_roots) in scopes {
        differ.match_scope(old_roots, new_roots);
    }
    differ.match_moves();

    // Whatever is still unmatched is added or removed, with all it contains.
    let mut ops = Vec::new();
    let mut next = table.next_seq(lane).max(identities.first_new);
    // In the order of the files: new IDs follow the text.
    differ.leftover_new.sort_unstable();
    for root in std::mem::take(&mut differ.leftover_new) {
        if differ.assigned[root].is_none() {
            differ.add(root, lane, &mut next, &mut ops);
        }
    }
    // Moves, outermost destination first: when a symbol arrives, what will
    // contain it is already in place (and has an ID, if it is new).
    let mut moves = std::mem::take(&mut differ.moves);
    moves.sort_by_key(|(new, _)| differ.new_depth(*new));
    for (new, mut op) in moves {
        if let OpKind::Move { parent, .. } = &mut op {
            *parent = differ.news[new].parent.and_then(|p| differ.assigned[p]);
        }
        ops.push(op);
    }
    ops.append(&mut differ.changes);
    for root in std::mem::take(&mut differ.leftover_old) {
        differ.remove(root, &mut ops);
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
            // Known symbols by (parent, kind, name), each queue in order of
            // ID: namesakes are recognized in the order they were created.
            let mut known: HashMap<(Option<SymbolId>, kito_core::SymbolKind, &str), VecDeque<SymbolId>> =
                HashMap::new();
            for (id, entry) in &entries {
                known.entry((entry.parent, entry.kind, entry.name.as_str())).or_default().push_back(*id);
            }
            let mut ids: Vec<Option<SymbolId>> = vec![None; old.parsed.symbols.len()];
            for (i, sym) in old.parsed.symbols.iter().enumerate() {
                let parent = match sym.parent {
                    None => None,
                    Some(p) => match ids[p] {
                        Some(id) => Some(id),
                        None => continue, // the parent is unknown to the history, so is this
                    },
                };
                let id = known.get_mut(&(parent, sym.kind, sym.name.as_str())).and_then(VecDeque::pop_front);
                if let Some(id) = id {
                    seen.insert(id, (sym, old.content));
                    ids[i] = Some(id);
                }
            }
        }
        let base = self.olds.len();
        let index: HashMap<SymbolId, usize> =
            entries.iter().enumerate().map(|(i, (id, _))| (*id, base + i)).collect();
        for (id, entry) in &entries {
            let parent = entry.parent.and_then(|p| index.get(&p).copied());
            self.old_at.insert(*id, self.olds.len());
            self.olds.push(Old { id: *id, entry, seen: seen.get(id).copied(), parent, children: Vec::new() });
            self.matched.push(false);
        }
        let mut roots = Vec::new();
        for i in base..self.olds.len() {
            match self.olds[i].parent {
                Some(parent) => self.olds[parent].children.push(i),
                None => roots.push(i),
            }
        }
        roots
    }

    /// Loads the symbols of the new content. Returns the top-level ones.
    fn load_new(&mut self, change: &FileChange<'a>, identities: &Identities) -> Vec<usize> {
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
            // An identity can be given once: a second claim to it is ignored.
            let known =
                identities.get(change.path, sym.range.start).filter(|id| !self.known.contains(&Some(*id)));
            self.known.push(known);
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
    /// each matched pair.
    fn match_scope(&mut self, olds: Vec<usize>, news: Vec<usize>) {
        let mut free_old = olds;
        let mut free_new = news;
        let mut pairs: Vec<(usize, usize)> = Vec::new();

        // Pass 0: identities known in advance. A new symbol known to be an
        // old one of this scope is that one; known to be one from elsewhere,
        // or one the table has never seen, it waits for the moves and the
        // additions. An old symbol claimed from elsewhere waits too.
        free_new.retain(|&n| {
            let Some(id) = self.known[n] else { return true };
            match self.old_at.get(&id).and_then(|o| free_old.iter().position(|x| x == o)) {
                Some(at) => pairs.push((free_old.remove(at), n)),
                None => self.leftover_new.push(n),
            }
            false
        });
        free_old.retain(|&o| {
            let claimed = self.claimed.contains(&self.olds[o].id);
            if claimed {
                self.leftover_old.push(o);
            }
            !claimed
        });

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

        // Pass 2: same body, different name, and no other candidate. A small
        // body says little on its own (`{}` is everywhere), so for those the
        // signature must be the same too, and no other new symbol may look
        // the same.
        let alike = |new: &ParsedSymbol, seen: &ParsedSymbol| {
            seen.body == new.body && (new.body_tokens >= MIN_BODY_TOKENS || seen.signature == new.signature)
        };
        let snapshot = free_new.clone();
        free_new.retain(|&n| {
            let sym = self.news[n].sym;
            if sym.body_tokens < MIN_BODY_TOKENS {
                let twins = snapshot.iter().filter(|&&m| {
                    let other = self.news[m].sym;
                    other.kind == sym.kind && other.body == sym.body && other.signature == sym.signature
                });
                if twins.count() != 1 {
                    return true;
                }
            }
            let same_body = |o: &usize| {
                let old = &self.olds[*o];
                old.entry.kind == sym.kind && old.seen.is_some_and(|(seen, _)| alike(sym, seen))
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
            self.pair(o, n, false);
        }
    }

    /// Records that an old and a new symbol are the same one, emits what
    /// changed between them, and matches their members. `moved`: the symbol
    /// changed file or enclosing symbol on its own (its members, and the
    /// symbols matched inside a scope, follow their container without a
    /// `Move` of their own).
    fn pair(&mut self, old: usize, new: usize, moved: bool) {
        let id = self.olds[old].id;
        self.matched[old] = true;
        self.assigned[new] = Some(id);
        let (entry, seen) = (self.olds[old].entry, self.olds[old].seen);
        let (file, sym, content) = (self.news[new].file, self.news[new].sym, self.news[new].content);

        if entry.name != sym.name {
            self.changes.push(OpKind::Rename { id, from: entry.name.clone(), to: sym.name.clone() });
        }
        if moved {
            let from = match seen {
                Some((seen, _)) => location(&entry.file, seen),
                None => Location { file: entry.file.clone(), line: 0, start: 0, end: 0 },
            };
            // The new parent is filled in at the end: it may not have an ID yet.
            self.moves.push((new, OpKind::Move { id, from, to: location(file, sym), parent: None }));
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

        // Members that a move already took elsewhere are not part of the scope.
        let old_members: Vec<usize> =
            self.olds[old].children.iter().copied().filter(|&o| !self.matched[o]).collect();
        let new_members: Vec<usize> =
            self.news[new].children.iter().copied().filter(|&n| self.assigned[n].is_none()).collect();
        if !old_members.is_empty() || !new_members.is_empty() {
            self.match_scope(old_members, new_members);
        }
    }

    fn old_depth(&self, old: usize) -> usize {
        std::iter::successors(self.olds[old].parent, |&p| self.olds[p].parent).count()
    }

    fn new_depth(&self, new: usize) -> usize {
        std::iter::successors(self.news[new].parent, |&p| self.news[p].parent).count()
    }

    /// `true` if the new symbol is in another file or under another parent
    /// than the old one. Symbols of the same scope had their chance in
    /// [`Differ::match_scope`], with stricter rules.
    fn scope_changed(&self, old: usize, new: usize) -> bool {
        let (o, n) = (&self.olds[old], &self.news[new]);
        if o.entry.file != n.file {
            return true;
        }
        match n.parent {
            None => o.entry.parent.is_some(),
            // A parent without an ID is a new symbol: not where the old one was.
            Some(p) => self.assigned[p].is_none_or(|id| o.entry.parent != Some(id)),
        }
    }

    /// For the symbols too small to be recognized by their body: the two
    /// enclosing scopes are the same kind of place.
    fn scopes_alike(&self, old: usize, new: usize) -> bool {
        use kito_core::SymbolKind::Module;
        match (self.olds[old].parent, self.news[new].parent) {
            (None, None) => true,
            (Some(p), Some(q)) => self.olds[p].entry.name == self.news[q].sym.name,
            // In or out of a namespace.
            (Some(p), None) => self.olds[p].entry.kind == Module,
            (None, Some(q)) => self.news[q].sym.kind == Module,
        }
    }

    /// How confident we are that an old symbol and a new one somewhere else
    /// are the same one. `None`: not the same.
    fn move_score(&self, old: usize, new: usize) -> Option<f32> {
        let (o, n) = (&self.olds[old], self.news[new].sym);
        let (seen, _) = o.seen?;
        if o.entry.kind != n.kind {
            return None;
        }
        let same_name = o.entry.name == n.name;
        let same_body = seen.body == n.body;
        let score = if same_body && n.body_tokens >= MIN_BODY_TOKENS {
            Some(1.0)
        } else if same_body && same_name && seen.signature == n.signature && self.scopes_alike(old, new) {
            // Small, but identical down to the name.
            Some(1.0)
        } else if same_name && seen.tokens >= MIN_SHAPE_TOKENS && n.tokens >= MIN_SHAPE_TOKENS {
            let similarity = seen.fingerprint.similarity(&n.fingerprint);
            (similarity >= SIMILARITY).then_some(similarity)
        } else if same_name {
            let overlap = self.member_overlap(old, new);
            (overlap >= MEMBER_OVERLAP).then_some(overlap)
        } else {
            None
        };
        // At equal evidence, the one that kept its name.
        score.map(|score| score + if same_name { 0.5 } else { 0.0 })
    }

    /// Moves: a symbol that disappeared from one place and one that appeared
    /// in another (another file, or another enclosing symbol), with the same
    /// body, or with the same name and a very similar shape. Everything no
    /// scope matched is a candidate, at any depth; containers pair up before
    /// what they contain, so that a namespace that moved takes its classes
    /// along instead of losing them one by one.
    fn match_moves(&mut self) {
        // First the identities known in advance, outermost first: a known
        // container settles what it contains through its own scope.
        let mut known: Vec<(usize, usize, usize)> = (0..self.news.len())
            .filter_map(|n| {
                let o = *self.old_at.get(&self.known[n]?)?;
                Some((self.old_depth(o) + self.new_depth(n), o, n))
            })
            .collect();
        known.sort_unstable();
        for (_, o, n) in known {
            if !self.matched[o] && self.assigned[n].is_none() {
                self.pair(o, n, self.scope_changed(o, n));
            }
        }

        // New symbols by what could identify them.
        let mut by_body: HashMap<kito_core::Hash, Vec<usize>> = HashMap::new();
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (n, new) in self.news.iter().enumerate() {
            if self.assigned[n].is_some() || self.known[n].is_some() {
                continue;
            }
            if new.sym.body_tokens >= MIN_BODY_TOKENS {
                by_body.entry(new.sym.body).or_default().push(n);
            }
            by_name.entry(new.sym.name.as_str()).or_default().push(n);
        }

        let mut candidates: Vec<(usize, f32, usize, usize)> = Vec::new();
        for (o, old) in self.olds.iter().enumerate() {
            let Some((seen, _)) = old.seen else { continue };
            if self.matched[o] || self.claimed.contains(&old.id) {
                continue;
            }
            let same_body = by_body.get(&seen.body).into_iter().flatten();
            let same_name = by_name.get(old.entry.name.as_str()).into_iter().flatten();
            let mut tried: Vec<usize> = same_body.chain(same_name).copied().collect();
            tried.sort_unstable();
            tried.dedup();
            for n in tried {
                if !self.scope_changed(o, n) {
                    continue;
                }
                if let Some(score) = self.move_score(o, n) {
                    candidates.push((self.old_depth(o) + self.new_depth(n), score, o, n));
                }
            }
        }
        candidates
            .sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
        for (_, _, o, n) in candidates {
            // An earlier pair may have settled either of the two, or brought
            // them into the same scope (where they were then told apart).
            if self.matched[o] || self.assigned[n].is_some() || !self.scope_changed(o, n) {
                continue;
            }
            self.pair(o, n, true);
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
    /// What a move took elsewhere stays.
    fn remove(&self, old: usize, ops: &mut Vec<OpKind>) {
        if self.matched[old] {
            return;
        }
        for &member in &self.olds[old].children {
            self.remove(member, ops);
        }
        ops.push(OpKind::RemoveSymbol { id: self.olds[old].id });
    }

    /// `AddSymbol` for a symbol and everything new it contains, container first.
    fn add(&mut self, new: usize, lane: LaneId, next: &mut u64, ops: &mut Vec<OpKind>) {
        // A symbol known in advance keeps the ID it was born with (in
        // another lane); any other gets the next one of this lane.
        let (file, sym, content, lang) =
            (self.news[new].file, self.news[new].sym, self.news[new].content, self.news[new].parsed.lang);
        let parent = self.news[new].parent.and_then(|p| self.assigned[p]);
        let free =
            |id: &SymbolId, given: &HashSet<SymbolId>| self.table.get(*id).is_none() && !given.contains(id);
        let id = match self.known[new].filter(|id| free(id, &self.given)) {
            Some(id) => id,
            // A symbol that was here under this name and went away: it is back.
            None => {
                let back = self
                    .gone
                    .get_mut(&(file.to_owned(), parent, sym.kind, sym.name.clone()))
                    .and_then(|ids| {
                        while let Some(id) = ids.pop_front() {
                            if self.table.get(id).is_none() && !self.given.contains(&id) {
                                return Some(id);
                            }
                        }
                        None
                    });
                match back {
                    Some(id) => id,
                    None => {
                        *next += 1;
                        SymbolId::new(lane, *next - 1)
                    }
                }
            }
        };
        self.given.insert(id);
        self.assigned[new] = Some(id);
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
            // A member with an ID came from elsewhere: its `Move` says so.
            if self.assigned[member].is_none() {
                self.add(member, lane, next, ops);
            }
        }
    }
}
