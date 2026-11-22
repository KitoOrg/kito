//! The scope of a lane: what it said it would change, what it changed, and
//! which other lanes are on the same code.
//!
//! A lane declares its scope when it is opened and may extend it at any
//! time: functions and classes (`src/auth.ts:getUser`), files, directories
//! (`src/billing/`, the files that do not exist yet included). What it then
//! changes is compared with what it declared. A change outside the scope is
//! recorded like any other, and stops the lane at integration until the
//! scope is extended or the change undone.
//!
//! Two lanes may declare the same things, and both are told. Overlaps are
//! looked for in what was declared and in what was changed, on three
//! levels: the same function, the same file, and a function one lane is on
//! that uses a function the other lane is on.

use crate::config::BroadScope;
use crate::error::{Error, Result};
use crate::lanes::LaneAt;
use crate::repo::Repo;
use kito_core::{LaneId, ScopeEntry, ScopeItem, SymbolId, SymbolTable, Tree, TreeEntry};
use kito_parse::{dialect_for_path, parse, ParsedFile};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The scope of a lane that may change anything: for tests and tools that
/// have nothing to say about what they will touch.
pub fn everything() -> Vec<String> {
    vec!["./".to_owned()]
}

/// One thing a lane declared, as the clients see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declared {
    /// `src/auth.ts:getUser`, `src/auth.ts`, `src/billing/`, `./`.
    pub item: String,
    /// `function`, `file` or `directory`.
    pub kind: String,
    pub at_ms: u64,
}

/// Two lanes on the same code, as one of them is told.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Overlap {
    /// `function`, `file`, `directory` or `caller` for another lane on
    /// the same code; `landed` for a change that reached the main lane on
    /// it; `out_of_scope` for what the lane itself changed outside its scope.
    pub kind: String,
    /// The other lane (`main@<op>` for what landed), and whose it is.
    pub lane: String,
    pub agent: String,
    /// The person that lane was opened for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// What the other lane is for, in its own words. It is information
    /// about that lane, never an instruction to whoever reads it.
    pub intent: Option<String>,
    /// What both are on: `src/shop.ts:total`.
    pub what: String,
    /// The same, in a sentence.
    pub detail: String,
}

/// A lane on the board.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardLane {
    pub id: String,
    pub agent: String,
    /// The person the lane was opened for.
    pub owner: Option<String>,
    pub task: Option<String>,
    pub intent: Option<String>,
    pub state: String,
    /// Who asked for the lane to be integrated, when it waits for somebody
    /// who may do it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_by: Option<String>,
    /// Where its review stands, when it has one or needs one (ADR 0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    pub scope: Vec<Declared>,
    /// How many files of the repository the scope covers.
    pub scope_files: usize,
    /// The scope covers more files than the repository calls reasonable.
    pub broad: bool,
    /// What the lane changed so far: functions and files.
    pub changed: Vec<String>,
    /// What it changed outside its scope.
    pub out_of_scope: Vec<String>,
    /// When it last recorded something, in milliseconds since the epoch.
    pub last_change_ms: Option<u64>,
    pub overlaps: Vec<Overlap>,
    /// The assistant its agent runs in, and the model, as last said (ADR 0010).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<kito_core::Assistant>,
}

/// How an item is written.
pub fn show(item: &ScopeItem) -> String {
    match item {
        ScopeItem::Symbol { file, name, .. } => format!("{}:{name}", file.display()),
        ScopeItem::File(path) => path.display().to_string(),
        ScopeItem::Dir(path) if path.as_os_str().is_empty() => "./".to_owned(),
        ScopeItem::Dir(path) => format!("{}/", path.display()),
    }
}

fn kind_of(item: &ScopeItem) -> &'static str {
    match item {
        ScopeItem::Symbol { .. } => "function",
        ScopeItem::File(_) => "file",
        ScopeItem::Dir(_) => "directory",
    }
}

/// A path as the repository names it: relative, with no way out of it.
pub(crate) fn clean(text: &str) -> Result<PathBuf> {
    // An agent on Windows may write `src\auth.ts` (ADR 0002).
    let text = kito_core::paths::portable_text(text);
    let mut text = text.trim();
    while let Some(rest) = text.strip_prefix("./") {
        text = rest;
    }
    let text = text.trim_end_matches('/');
    let path = Path::new(text);
    let plain = path.components().all(|part| matches!(part, std::path::Component::Normal(_)));
    if text.is_empty() || !plain {
        return Err(Error::Other(format!("{text:?} is not a path inside the repository")));
    }
    Ok(path.to_owned())
}

/// Reads one item of a scope: `file:name` is a function or a class, a path
/// that ends with `/` (or names a directory of the repository) is a
/// directory, anything else is a file.
pub(crate) fn parse_item(tree: &Tree, symbols: &SymbolTable, text: &str) -> Result<ScopeItem> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Other("an empty scope item: give a function, a file or a directory".into()));
    }
    if matches!(text, "." | "./") {
        return Ok(ScopeItem::Dir(PathBuf::new()));
    }
    if text.ends_with('/') {
        return Ok(ScopeItem::Dir(clean(text)?));
    }
    if let Some((file, name)) = text.split_once(':') {
        if !file.is_empty() && !name.is_empty() && !name.contains('/') {
            let file = clean(file)?;
            let id = match symbols.find(&file, name).as_slice() {
                [one] => Some(*one),
                _ => None,
            };
            return Ok(ScopeItem::Symbol { file, name: name.to_owned(), id });
        }
    }
    let path = clean(text)?;
    match !tree.contains(&path) && tree.under(&path).next().is_some() {
        true => Ok(ScopeItem::Dir(path)),
        false => Ok(ScopeItem::File(path)),
    }
}

/// The files under a directory; under the empty path, all of them.
fn under<'a>(tree: &'a Tree, dir: &'a Path) -> Box<dyn Iterator<Item = &'a Path> + 'a> {
    match dir.as_os_str().is_empty() {
        true => Box::new(tree.iter().map(|(path, _)| path)),
        false => Box::new(tree.under(dir).map(|(path, _)| path)),
    }
}

fn covers_file(items: &[&ScopeItem], path: &Path) -> bool {
    items.iter().any(|item| match item {
        ScopeItem::Dir(dir) => path.starts_with(dir),
        ScopeItem::File(file) => file == path,
        ScopeItem::Symbol { .. } => false,
    })
}

/// A symbol is in the scope if it was declared, by what it is or by its
/// file and name, or if something that encloses it was.
fn covers_symbol(
    items: &[&ScopeItem],
    tables: [&SymbolTable; 2],
    id: Option<SymbolId>,
    file: &Path,
    name: &str,
) -> bool {
    let (mut id, mut name) = (id, name.to_owned());
    for _ in 0..64 {
        let plain = name.rsplit('.').next().unwrap_or(&name).to_owned();
        let declared = items.iter().any(|item| match item {
            ScopeItem::Symbol { file: f, name: n, id: i } => {
                (i.is_some() && *i == id) || (f == file && (*n == name || *n == plain))
            }
            _ => false,
        });
        if declared {
            return true;
        }
        // What encloses it: by the tables when they know it, by its name
        // otherwise (`Session.refresh` is inside `Session`).
        let known =
            id.and_then(|id| tables.iter().find_map(|table| table.get(id).map(|entry| (*table, entry))));
        match known {
            Some((table, entry)) => match entry.parent {
                Some(parent) => {
                    name = table.qualified_name(parent).unwrap_or_default();
                    id = Some(parent);
                }
                None => return false,
            },
            None => match name.rsplit_once('.') {
                Some((outer, _)) => {
                    name = outer.to_owned();
                    id = None;
                }
                None => return false,
            },
        }
    }
    false
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// The text with every whole-word `old` written as `new`.
fn with_names(text: &[u8], renames: &[(String, String)]) -> Vec<u8> {
    let mut text = text.to_vec();
    for (old, new) in renames {
        let (old, mut out, mut i) = (old.as_bytes(), Vec::with_capacity(text.len()), 0);
        while i < text.len() {
            let hit = text[i..].starts_with(old)
                && !i.checked_sub(1).is_some_and(|p| is_word(text[p]))
                && !text.get(i + old.len()).is_some_and(|c| is_word(*c));
            match hit {
                true => {
                    out.extend_from_slice(new.as_bytes());
                    i += old.len();
                }
                false => {
                    out.push(text[i]);
                    i += 1;
                }
            }
        }
        text = out;
    }
    text
}

/// The text of a symbol without the symbols it contains.
fn own_text(source: &[u8], file: &ParsedFile, index: usize) -> Vec<u8> {
    let symbol = &file.symbols[index];
    let (mut out, mut at) = (Vec::new(), symbol.range.start);
    for child in file.symbols.iter().filter(|s| s.parent == Some(index)) {
        out.extend_from_slice(&source[at..child.range.start.max(at)]);
        at = child.range.end.max(at);
    }
    out.extend_from_slice(&source[at..symbol.range.end.max(at)]);
    out
}

/// The text of a file outside its symbols: imports, statements at the top.
fn rest_text(source: &[u8], file: &ParsedFile) -> Vec<u8> {
    let (mut out, mut at) = (Vec::new(), 0);
    for symbol in file.symbols.iter().filter(|s| s.parent.is_none()) {
        out.extend_from_slice(&source[at..symbol.range.start.max(at)]);
        at = symbol.range.end.max(at);
    }
    out.extend_from_slice(&source[at.min(source.len())..]);
    out
}

/// One side of a file that changed: its bytes and, when it reads cleanly,
/// its symbols with who they are.
struct Side {
    source: Vec<u8>,
    parsed: Option<(ParsedFile, Vec<Option<SymbolId>>)>,
}

/// A part of a file a lane changed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    /// A file that is not read as symbols, as a whole.
    Whole,
    /// The text outside the symbols.
    Rest,
    Symbol {
        id: Option<SymbolId>,
        name: String,
    },
}

/// A function a lane declared or changed.
#[derive(Clone, Debug)]
struct SymbolClaim {
    id: Option<SymbolId>,
    file: PathBuf,
    name: String,
    declared: bool,
    changed: bool,
}

/// A function of a lane that uses another function.
#[derive(Clone, Debug)]
struct Use {
    /// `src/billing.ts:invoice`.
    from: String,
    file: PathBuf,
    to: SymbolId,
}

/// What a lane is on: what it declared and what it changed.
#[derive(Clone, Debug, Default)]
pub(crate) struct Claims {
    symbols: Vec<SymbolClaim>,
    /// Files claimed as such: (declared, changed).
    files: BTreeMap<PathBuf, (bool, bool)>,
    dirs: Vec<PathBuf>,
    uses: Vec<Use>,
    changed: Vec<String>,
    pub(crate) out_of_scope: Vec<String>,
    scope_files: usize,
}

/// What the claims of a lane were computed on: they hold until it changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaimsAt {
    ops: u64,
    base: kito_core::OpId,
    scope: usize,
}

pub(crate) type ClaimsCache = std::sync::Mutex<HashMap<LaneId, (ClaimsAt, Arc<Claims>)>>;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// `true` for an op that says what a change meant for a symbol.
fn is_symbolic(op: &str) -> bool {
    matches!(op, "AddSymbol" | "RemoveSymbol" | "ModifyBody" | "ModifySignature" | "Rename" | "Move")
}

fn last_word(name: &str) -> &str {
    let cut = name.bytes().rposition(|c| !is_word(c)).map_or(0, |i| i + 1);
    &name[cut..]
}

fn same_symbol(a: &SymbolClaim, b: &SymbolClaim) -> bool {
    match (a.id, b.id) {
        (Some(a), Some(b)) => a == b,
        _ => a.file == b.file && a.name == b.name,
    }
}

/// One symbol is inside the other: `Registry.add` and `Registry`.
fn nested(a: &SymbolClaim, b: &SymbolClaim) -> bool {
    a.file == b.file
        && (a.name.strip_prefix(b.name.as_str()).is_some_and(|rest| rest.starts_with('.'))
            || b.name.strip_prefix(a.name.as_str()).is_some_and(|rest| rest.starts_with('.')))
}

fn did(claim: &SymbolClaim) -> &'static str {
    match (claim.declared, claim.changed) {
        (true, true) => "declared and changed",
        (false, true) => "changed",
        _ => "declared",
    }
}

impl Repo {
    fn lane_at(&self, lane: LaneId) -> Result<&LaneAt> {
        self.lanes.get(&lane).ok_or(Error::NoSuchLane(lane))
    }

    /// `true` for a file any lane may change without declaring it.
    fn always_allowed(&self, rules: &crate::ignore::IgnoreRules, path: &Path) -> bool {
        !self.config.scope.always_allowed.is_empty() && rules.is_ignored(path, false)
    }

    fn side(&self, entry: Option<&TreeEntry>, table: &SymbolTable, path: &Path) -> Result<Option<Side>> {
        let Some(entry) = entry else { return Ok(None) };
        let source = self.store.get_blob(&entry.blob)?.unwrap_or_default();
        let parsed = dialect_for_path(path)
            .and_then(|dialect| parse(dialect, &source))
            .filter(|file| !file.has_errors)
            .map(|file| {
                let ids = kito_diff::ast::identify(table, path, &file);
                (file, ids)
            });
        Ok(Some(Side { source, parsed }))
    }

    /// The parts of one file a lane changed, a rename it made itself and
    /// what follows from it aside.
    fn parts_of(&self, at: &LaneAt, path: &Path, renames: &[(String, String)]) -> Result<Vec<Part>> {
        self.parts_between(
            (&at.base_tree, &at.base_symbols),
            (&at.work.tree, &at.work.symbols),
            path,
            renames,
        )
    }

    /// The parts of one file that differ between two states of the
    /// repository, the text that only differs by `renames` aside.
    fn parts_between(
        &self,
        before: (&Tree, &SymbolTable),
        after: (&Tree, &SymbolTable),
        path: &Path,
        renames: &[(String, String)],
    ) -> Result<Vec<Part>> {
        let before = self.side(before.0.get(path), before.1, path)?;
        let after = self.side(after.0.get(path), after.1, path)?;
        let unread = |side: &Option<Side>| side.as_ref().is_some_and(|side| side.parsed.is_none());
        if unread(&before) || unread(&after) {
            // Not read as symbols on one side: the file as a whole.
            let same = match (&before, &after) {
                (Some(before), Some(after)) => with_names(&before.source, renames) == after.source,
                _ => false,
            };
            return Ok(if same { Vec::new() } else { vec![Part::Whole] });
        }
        let empty =
            (ParsedFile { lang: kito_core::Lang::Rust, has_errors: false, symbols: Vec::new() }, Vec::new());
        let (old_source, (old, old_ids)) = match &before {
            Some(side) => (side.source.as_slice(), side.parsed.as_ref().unwrap_or(&empty)),
            None => (&[][..], &empty),
        };
        let (new_source, (new, new_ids)) = match &after {
            Some(side) => (side.source.as_slice(), side.parsed.as_ref().unwrap_or(&empty)),
            None => (&[][..], &empty),
        };
        let mut parts = Vec::new();
        // The symbols as they are now, against what each was.
        let mut met = vec![false; old.symbols.len()];
        for (i, id) in new_ids.iter().enumerate() {
            let name = new.qualified_name(i);
            let was = match *id {
                Some(id) => old_ids.iter().position(|other| *other == Some(id)),
                None => (0..old.symbols.len())
                    .find(|&j| !met[j] && old_ids[j].is_none() && old.qualified_name(j) == name),
            };
            let changed = match was {
                // By its text, comments included: what was edited was edited.
                Some(j) => {
                    met[j] = true;
                    with_names(&own_text(old_source, old, j), renames) != own_text(new_source, new, i)
                }
                None => true,
            };
            if changed {
                parts.push(Part::Symbol { id: *id, name });
            }
        }
        // The ones that are no longer in this file.
        for (j, gone) in met.iter().enumerate() {
            if !gone {
                parts.push(Part::Symbol { id: old_ids[j], name: old.qualified_name(j) });
            }
        }
        if with_names(&rest_text(old_source, old), renames) != rest_text(new_source, new) {
            parts.push(Part::Rest);
        }
        if parts.is_empty() && before.is_some() != after.is_some() {
            // A file with nothing in it, added or removed.
            parts.push(Part::Whole);
        }
        Ok(parts)
    }

    /// What a lane is on, computed once for each state it is in.
    pub(crate) fn claims(&self, lane: LaneId) -> Result<Arc<Claims>> {
        let at = self.lane_at(lane)?;
        let key = ClaimsAt { ops: at.work.ops, base: at.record.base_head, scope: at.record.scope.len() };
        if let Some((known, claims)) = self.claims_cache.lock().unwrap_or_else(|e| e.into_inner()).get(&lane)
        {
            if *known == key {
                return Ok(claims.clone());
            }
        }
        let claims = Arc::new(self.compute_claims(at)?);
        self.claims_cache.lock().unwrap_or_else(|e| e.into_inner()).insert(lane, (key, claims.clone()));
        Ok(claims)
    }

    fn compute_claims(&self, at: &LaneAt) -> Result<Claims> {
        let items: Vec<&ScopeItem> = at.record.scope.iter().map(|entry| &entry.item).collect();
        let tables = [&at.work.symbols, &at.base_symbols];
        let mut claims = Claims::default();

        // What was declared.
        for item in &items {
            match item {
                ScopeItem::Symbol { file, name, id } => {
                    // One declared before it existed may exist by now.
                    let id = id.or_else(|| match at.work.symbols.find(file, name).as_slice() {
                        [one] => Some(*one),
                        _ => None,
                    });
                    // Told by the name it has today.
                    let (file, name) = id
                        .and_then(|id| at.work.symbols.get(id).map(|entry| (id, entry)))
                        .map(|(id, entry)| {
                            (
                                entry.file.clone(),
                                at.work.symbols.qualified_name(id).unwrap_or_else(|| name.clone()),
                            )
                        })
                        .unwrap_or_else(|| (file.clone(), name.clone()));
                    claims.symbols.push(SymbolClaim { id, file, name, declared: true, changed: false });
                }
                ScopeItem::File(path) => claims.files.entry(path.clone()).or_default().0 = true,
                ScopeItem::Dir(dir) => claims.dirs.push(dir.clone()),
            }
        }
        let mut covered: BTreeSet<&Path> = BTreeSet::new();
        for item in &items {
            match item {
                ScopeItem::Symbol { file, .. } => {
                    covered.insert(file);
                }
                ScopeItem::File(path) => {
                    covered.insert(path);
                }
                ScopeItem::Dir(dir) => covered.extend(under(&at.base_tree, dir)),
            }
        }
        claims.scope_files = covered.len();

        // What was changed, a rename and what follows from it aside.
        let renames: Vec<(String, String)> = at
            .work
            .symbols
            .changed(&at.base_symbols)
            .filter_map(|id| Some((at.base_symbols.get(id)?, at.work.symbols.get(id)?)))
            .filter(|(was, is)| was.name != is.name)
            .map(|(was, is)| (last_word(&was.name).to_owned(), last_word(&is.name).to_owned()))
            .filter(|(was, is)| !was.is_empty() && was != is)
            .collect();
        let allowed = crate::ignore::IgnoreRules::new(&at.work.root, &self.config.scope.always_allowed);
        let changed: Vec<PathBuf> = at.base_tree.differences(&at.work.tree).map(Path::to_owned).collect();
        for path in &changed {
            if self.always_allowed(&allowed, path) {
                continue;
            }
            let parts = self.parts_of(at, path, &renames)?;
            let in_file = covers_file(&items, path);
            let mut inside = Vec::new();
            for part in &parts {
                let (told, within) = match part {
                    Part::Whole => (path.display().to_string(), in_file),
                    // Decided below: it depends on the symbols of the file.
                    Part::Rest => (format!("{} (outside its functions)", path.display()), in_file),
                    Part::Symbol { id, name } => {
                        let within = in_file || covers_symbol(&items, tables, *id, path, name);
                        let known = claims.symbols.iter_mut().find(|claim| match (claim.id, id) {
                            (Some(a), Some(b)) => a == *b,
                            _ => claim.file == *path && claim.name == *name,
                        });
                        match known {
                            Some(claim) => claim.changed = true,
                            None => claims.symbols.push(SymbolClaim {
                                id: *id,
                                file: path.clone(),
                                name: name.clone(),
                                declared: false,
                                changed: true,
                            }),
                        }
                        (format!("{}:{name}", path.display()), within)
                    }
                };
                inside.push((part, told, within));
            }
            // The text around the functions of a file may change with them:
            // an import follows the function that needs it.
            let on_its_symbols = inside
                .iter()
                .any(|(part, _, within)| matches!(part, Part::Symbol { .. }) && *within)
                || items.iter().any(|item| matches!(item, ScopeItem::Symbol { file, .. } if file == path));
            // A file that came or went is told as the file, not part by part.
            let whole = at.base_tree.contains(path) != at.work.tree.contains(path);
            let mut left_out = false;
            for (part, told, within) in inside {
                let within = within || (*part == Part::Rest && on_its_symbols);
                if !matches!(part, Part::Symbol { .. }) {
                    claims.files.entry(path.clone()).or_default().1 = true;
                }
                left_out |= !within;
                if !within && !whole {
                    claims.out_of_scope.push(told.clone());
                }
                if !whole || matches!(part, Part::Symbol { .. }) {
                    claims.changed.push(told);
                }
            }
            if whole {
                claims.changed.push(path.display().to_string());
                claims.files.entry(path.clone()).or_default().1 = true;
                if left_out {
                    claims.out_of_scope.push(path.display().to_string());
                }
            }
        }

        // What its functions use, as far as the syntax tells.
        let mut by_file: BTreeMap<&Path, Vec<SymbolId>> = BTreeMap::new();
        for claim in &claims.symbols {
            if let Some(id) = claim.id.filter(|id| at.work.symbols.get(*id).is_some()) {
                by_file.entry(claim.file.as_path()).or_default().push(id);
            }
        }
        let mut sources: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        for file in by_file.keys() {
            if let Some(entry) = at.work.tree.get(file) {
                sources.insert((*file).to_owned(), self.store.get_blob(&entry.blob)?.unwrap_or_default());
            }
        }
        let read = |path: &Path| sources.get(path).cloned();
        let overlay = BTreeMap::new();
        let snapshot = kito_semantic::Snapshot {
            root: &self.root,
            tree: &at.work.tree,
            symbols: &at.work.symbols,
            files: &read,
            overlay: &overlay,
        };
        let semantic = self.semantic();
        for (file, mine) in &by_file {
            for reference in semantic.heuristic.references_in(&snapshot, file) {
                // The use is the claimed function's, or of something inside it.
                let mut from = Some(reference.from);
                let mut owner = None;
                for _ in 0..64 {
                    let Some(id) = from else { break };
                    if mine.contains(&id) {
                        owner = Some(id);
                        break;
                    }
                    from = at.work.symbols.get(id).and_then(|entry| entry.parent);
                }
                let Some(owner) = owner else { continue };
                if reference.to == owner {
                    continue;
                }
                let name = at.work.symbols.qualified_name(owner).unwrap_or_default();
                let from = format!("{}:{name}", file.display());
                if !claims.uses.iter().any(|known| known.from == from && known.to == reference.to) {
                    claims.uses.push(Use { from, file: (*file).to_owned(), to: reference.to });
                }
            }
        }
        claims.changed.sort();
        claims.changed.dedup();
        claims.out_of_scope.sort();
        claims.out_of_scope.dedup();
        Ok(claims)
    }

    /// What a lane changed outside the scope it declared.
    pub fn out_of_scope(&self, lane: LaneId) -> Result<Vec<String>> {
        // A permanent lane holds whatever landed on it (ADR 0001).
        if self.lanes.get(&lane).is_some_and(|at| at.record.is_permanent()) {
            return Ok(Vec::new());
        }
        Ok(self.claims(lane)?.out_of_scope.clone())
    }

    /// What an edit about to be made in a lane would change outside what
    /// the lane declared: nothing, functions (`src/shop.py:discount`) or
    /// the file. `lines`: the lines of the file that will be replaced.
    /// With none, the file is judged as a whole: a lane that declared
    /// something in it may write it, and what it changed is judged when
    /// it is integrated.
    pub(crate) fn outside_scope(
        &self,
        lane: LaneId,
        path: &Path,
        lines: &[(u32, u32)],
    ) -> Result<Vec<String>> {
        let at = self.lane_at(lane)?;
        let items: Vec<&ScopeItem> = at.record.scope.iter().map(|entry| &entry.item).collect();
        let allowed = crate::ignore::IgnoreRules::new(&at.work.root, &self.config.scope.always_allowed);
        if self.always_allowed(&allowed, path) || covers_file(&items, path) {
            return Ok(Vec::new());
        }
        if !items.iter().any(|item| matches!(item, ScopeItem::Symbol { file, .. } if file == path)) {
            return Ok(vec![path.display().to_string()]);
        }
        if lines.is_empty() || !at.work.tree.contains(path) {
            return Ok(Vec::new());
        }
        // Functions of the file were declared: the lines say which ones
        // the edit is in. The text around them may change with them.
        let tables = [&at.work.symbols, &at.base_symbols];
        let spans = self.symbol_spans(lane, path)?;
        let mut outside = Vec::new();
        for (from, to) in lines {
            let on: Vec<&(SymbolId, u32, u32)> =
                spans.iter().filter(|(_, start, end)| start <= to && from <= end).collect();
            for (id, start, end) in &on {
                // The innermost ones: nothing inside them is on these lines too.
                let has_inner = on.iter().any(|(other, inner_start, inner_end)| {
                    other != id && start <= inner_start && inner_end <= end
                });
                if has_inner {
                    continue;
                }
                let name = at.work.symbols.qualified_name(*id).unwrap_or_default();
                if !covers_symbol(&items, tables, Some(*id), path, &name) {
                    outside.push(format!("{}:{name}", path.display()));
                }
            }
        }
        outside.sort();
        outside.dedup();
        Ok(outside)
    }

    /// The scope of a lane, as it declared it.
    pub fn lane_scope(&self, lane: LaneId) -> Result<Vec<Declared>> {
        Ok(self
            .lane_at(lane)?
            .record
            .scope
            .iter()
            .map(|entry| Declared {
                item: show(&entry.item),
                kind: kind_of(&entry.item).to_owned(),
                at_ms: entry.at_ms,
            })
            .collect())
    }

    /// Reads the items of a scope against a state of the repository, and
    /// refuses a scope the repository calls too broad.
    pub(crate) fn read_scope(
        &self,
        tree: &Tree,
        symbols: &SymbolTable,
        known: &[ScopeEntry],
        scope: &[String],
    ) -> Result<Vec<ScopeEntry>> {
        let mut entries: Vec<ScopeEntry> = Vec::new();
        for text in scope {
            let item = parse_item(tree, symbols, text)?;
            let again = |entry: &ScopeEntry| show(&entry.item) == show(&item);
            if !known.iter().any(again) && !entries.iter().any(again) {
                entries.push(ScopeEntry { item, at_ms: now_ms() });
            }
        }
        if self.config.scope.broad == BroadScope::Refuse {
            let mut files: BTreeSet<&Path> = BTreeSet::new();
            for entry in known.iter().chain(&entries) {
                if let ScopeItem::Dir(dir) = &entry.item {
                    files.extend(under(tree, dir));
                }
            }
            if files.len() > self.config.scope.broad_files {
                return Err(Error::Other(format!(
                    "this scope covers {} files, more than the {} this repository allows: name the \
                     functions, the files or narrower directories",
                    files.len(),
                    self.config.scope.broad_files
                )));
            }
        }
        Ok(entries)
    }

    /// Adds to the scope of a lane. What it declared before stays declared.
    pub fn extend_scope(&mut self, lane: LaneId, scope: &[String]) -> Result<Vec<Declared>> {
        if lane.is_main() {
            return Err(Error::Other(
                "the main lane has no scope: lanes declare what they will change".into(),
            ));
        }
        if scope.is_empty() {
            return Err(Error::Other("nothing to add: give a function, a file or a directory".into()));
        }
        self.scan_lane(lane)?;
        let at = self.lane_at(lane)?;
        let entries = self.read_scope(&at.work.tree, &at.work.symbols, &at.record.scope, scope)?;
        let at = self.lanes.get_mut(&lane).ok_or(Error::NoSuchLane(lane))?;
        at.record.scope.extend(entries);
        let state = at.record.state;
        self.put_lane_state(lane, state)?;
        self.lane_scope(lane)
    }

    /// The lanes on the same code as `mine`. `asked` is for a question about
    /// a function or a file, which is nobody's lane: the answer is worded
    /// about the thing, not to somebody.
    fn overlaps_of(&self, mine: &Claims, lane: LaneId, asked: bool) -> Result<Vec<Overlap>> {
        let mut found = Vec::new();
        for (other, at) in &self.lanes {
            if *other == lane {
                continue;
            }
            let theirs = self.claims(*other)?;
            let agent = at.record.agent.name.clone();
            let owner = at.record.owner.as_ref().map(|owner| owner.name.clone());
            let intent =
                at.work.intent.and_then(|id| self.store.intent(&id)).map(|intent| intent.summary.clone());
            let mut tell = |kind: &str, what: String, detail: String| {
                found.push(Overlap {
                    kind: kind.to_owned(),
                    lane: other.to_string(),
                    agent: agent.clone(),
                    owner: owner.clone(),
                    intent: intent.clone(),
                    what,
                    detail,
                });
            };
            // Files already told about by something more precise.
            let mut shared: BTreeSet<PathBuf> = BTreeSet::new();
            // The same function, or one inside the other.
            for a in &mine.symbols {
                for b in &theirs.symbols {
                    if same_symbol(a, b) || nested(a, b) {
                        shared.insert(b.file.clone());
                        shared.insert(a.file.clone());
                        let what = format!("{}:{}", b.file.display(), b.name);
                        let detail = match asked {
                            true => format!("{agent} {} it", did(b)),
                            false => format!("{agent} {} it; you {} it", did(b), did(a)),
                        };
                        tell("function", what, detail);
                    }
                }
            }
            // A function of one that uses a function of the other.
            let named = |claims: &'_ Claims, id: SymbolId| -> Option<(String, PathBuf, &'static str)> {
                claims.symbols.iter().find(|claim| claim.id == Some(id)).map(|claim| {
                    (format!("{}:{}", claim.file.display(), claim.name), claim.file.clone(), did(claim))
                })
            };
            for usage in &mine.uses {
                if let Some((target, file, did)) = named(&theirs, usage.to) {
                    shared.extend([file, usage.file.clone()]);
                    let detail = format!("your {} uses {target}, which {agent} {did}", usage.from);
                    tell("caller", format!("{} uses {target}", usage.from), detail);
                }
            }
            for usage in &theirs.uses {
                if let Some((target, file, did)) = named(mine, usage.to) {
                    shared.extend([file, usage.file.clone()]);
                    let detail = match asked {
                        true => format!("{agent}'s {} uses it", usage.from),
                        false => format!("{agent}'s {} uses {target}, which you {did}", usage.from),
                    };
                    tell("caller", format!("{} uses {target}", usage.from), detail);
                }
            }
            // The same file, on different parts of it.
            let files = |claims: &'_ Claims| -> BTreeSet<PathBuf> {
                claims
                    .files
                    .keys()
                    .cloned()
                    .chain(claims.symbols.iter().map(|claim| claim.file.clone()))
                    .collect()
            };
            let (my_files, their_files) = (files(mine), files(&theirs));
            let under = |dirs: &[PathBuf], path: &Path| dirs.iter().any(|dir| path.starts_with(dir));
            for file in my_files.iter().filter(|file| !shared.contains(*file)) {
                if their_files.contains(file) {
                    let detail = match asked {
                        true => format!("{agent} is in this file"),
                        false => format!("{agent} is in this file too, on other parts of it"),
                    };
                    tell("file", file.display().to_string(), detail);
                } else if under(&theirs.dirs, file) {
                    let detail = format!("{agent} declared the directory this file is in");
                    tell("file", file.display().to_string(), detail);
                }
            }
            for file in their_files.iter().filter(|file| !my_files.contains(*file) && !shared.contains(*file))
            {
                if under(&mine.dirs, file) {
                    let detail = match asked {
                        true => format!("{agent} is in this file"),
                        false => format!("{agent} is in this file, inside a directory you declared"),
                    };
                    tell("file", file.display().to_string(), detail);
                }
            }
            // Directories declared by both, one inside the other.
            for a in &mine.dirs {
                for b in &theirs.dirs {
                    if a.starts_with(b) || b.starts_with(a) {
                        let inner = if a.starts_with(b) { a } else { b };
                        let what = show(&ScopeItem::Dir(inner.clone()));
                        let detail = match asked {
                            true => format!("{agent} declared it"),
                            false => format!("{agent} declared it too"),
                        };
                        tell("directory", what, detail);
                    }
                }
            }
        }
        found.sort();
        found.dedup();
        Ok(found)
    }

    /// The other lanes that are on the same code as this one.
    pub fn overlaps(&self, lane: LaneId) -> Result<Vec<Overlap>> {
        let mine = self.claims(lane)?;
        self.overlaps_of(&mine, lane, false)
    }

    /// What reached the main lane after a lane started, among the things
    /// the lane declared or changed: the lane works on a state that no
    /// longer has the last word on them.
    pub fn landed(&self, lane: LaneId) -> Result<Vec<Overlap>> {
        let at = self.lane_at(lane)?;
        let claims = self.claims(lane)?;
        let in_dirs = |path: &Path| claims.dirs.iter().any(|dir| path.starts_with(dir));
        let mut found = Vec::new();
        let moved: Vec<&Path> = at.base_tree.differences(&self.main.tree).collect();
        for path in moved {
            let whole = claims.files.contains_key(path) || in_dirs(path);
            let mine: Vec<&SymbolClaim> = claims.symbols.iter().filter(|claim| claim.file == path).collect();
            if !whole && mine.is_empty() {
                continue;
            }
            let parts = self.parts_between(
                (&at.base_tree, &at.base_symbols),
                (&self.main.tree, &self.main.symbols),
                path,
                &[],
            )?;
            // Who changed it last on the main lane, and what for.
            let last = self.log(1, Some(path))?.into_iter().find_map(|item| match item {
                crate::repo::LogItem::Op { id, author, intent, op, .. } if !is_symbolic(&op) => {
                    Some((id, author, intent))
                }
                _ => None,
            });
            let (op, author, intent) = last.unwrap_or_default();
            for part in parts {
                let what = match &part {
                    Part::Symbol { id, name } => {
                        let asked = SymbolClaim {
                            id: *id,
                            file: path.to_owned(),
                            name: name.clone(),
                            declared: false,
                            changed: false,
                        };
                        match whole
                            || mine.iter().any(|claim| same_symbol(claim, &asked) || nested(claim, &asked))
                        {
                            true => format!("{}:{name}", path.display()),
                            false => continue,
                        }
                    }
                    Part::Rest | Part::Whole if whole => path.display().to_string(),
                    Part::Rest | Part::Whole => continue,
                };
                found.push(Overlap {
                    kind: "landed".to_owned(),
                    // Told once for each change that lands.
                    lane: format!("main@{}", &op[..op.len().min(8)]),
                    agent: author.clone(),
                    owner: None,
                    intent: intent.clone(),
                    detail: format!(
                        "{author} changed {what} on the main lane after your lane started: read it in the \
                         main directory before you go on"
                    ),
                    what,
                });
            }
        }
        found.sort();
        found.dedup();
        Ok(found)
    }

    /// What a lane has not been told yet: the overlaps that are new, what
    /// reached the main lane on its code, and what it changed outside its
    /// scope. Each is told once.
    pub fn take_notices(&mut self, lane: LaneId) -> Result<Vec<Overlap>> {
        let mut news = self.overlaps(lane)?;
        news.extend(self.landed(lane)?);
        for outside in self.out_of_scope(lane)? {
            news.push(Overlap {
                kind: "out_of_scope".to_owned(),
                lane: lane.to_string(),
                agent: self.lane_at(lane)?.record.agent.name.clone(),
                owner: None,
                intent: None,
                detail: format!(
                    "you changed {outside}, which is outside the scope you declared: extend the scope or \
                     undo the change, or the lane will not be integrated"
                ),
                what: outside,
            });
        }
        // What was said on the task the lane is for (ADR 0015).
        let task = self.lane_at(lane)?.record.task.as_deref().and_then(crate::tasks::number);
        if let Some(n) = task {
            let all = crate::tasks::load(&self.root);
            for comment in all.get(n).map(|task| task.comments.as_slice()).unwrap_or_default() {
                news.push(Overlap {
                    kind: "task_comment".to_owned(),
                    lane: crate::tasks::id(n),
                    agent: comment.by.clone(),
                    owner: None,
                    intent: None,
                    detail: format!(
                        "{} said on {}, which your lane is for: {}",
                        comment.by,
                        crate::tasks::id(n),
                        comment.text
                    ),
                    what: format!("comment {}", comment.n),
                });
            }
        }
        let told = self.told.entry(lane).or_default();
        news.retain(|overlap| told.insert(format!("{}|{}|{}", overlap.lane, overlap.kind, overlap.what)));
        Ok(news)
    }

    /// Every open lane: whose it is, what for, what it declared and changed,
    /// and who it overlaps with.
    pub fn board(&self) -> Result<Vec<BoardLane>> {
        let mut board = Vec::new();
        for (lane, at) in &self.lanes {
            let info = self.lane_info(*lane)?;
            let claims = self.claims(*lane)?;
            let last_change_ms = match at.work.ops > at.record.since {
                true => at
                    .work
                    .head
                    .and_then(|head| self.store.get_op(&head).ok().flatten())
                    .map(|op| op.at.wall_ms),
                false => None,
            };
            board.push(BoardLane {
                id: info.id,
                agent: info.agent,
                owner: info.owner,
                task: info.task,
                intent: info.intent,
                state: info.state,
                proposed_by: info.proposed_by,
                review: info.review,
                scope: self.lane_scope(*lane)?,
                scope_files: claims.scope_files,
                broad: claims.scope_files > self.config.scope.broad_files,
                changed: claims.changed.clone(),
                out_of_scope: claims.out_of_scope.clone(),
                last_change_ms,
                overlaps: self.overlaps_of(&claims, *lane, false)?,
                assistant: info.assistant,
            });
        }
        Ok(board)
    }

    /// The lanes that declared or changed a function, a file or a
    /// directory, given as a scope item is.
    pub fn who_touches(&self, target: &str) -> Result<Vec<Overlap>> {
        let item = parse_item(&self.main.tree, &self.main.symbols, target)?;
        let mut asked = Claims::default();
        match &item {
            ScopeItem::Symbol { file, name, id } => asked.symbols.push(SymbolClaim {
                id: *id,
                file: file.clone(),
                name: name.clone(),
                declared: true,
                changed: false,
            }),
            ScopeItem::File(path) => {
                asked.files.insert(path.clone(), (true, false));
            }
            ScopeItem::Dir(dir) => asked.dirs.push(dir.clone()),
        }
        self.overlaps_of(&asked, LaneId::MAIN, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_read_as_functions_files_and_directories() {
        let mut tree = Tree::new();
        for path in ["src/auth.ts", "src/billing/invoice.ts", "README.md"] {
            tree.apply(&kito_core::OpKind::FileAdd {
                path: PathBuf::from(path),
                blob: kito_core::BlobId::of(path.as_bytes()),
                mode: kito_core::FileMode::Regular,
            })
            .unwrap();
        }
        let symbols = SymbolTable::new();
        let read = |text: &str| parse_item(&tree, &symbols, text).map(|item| (show(&item), kind_of(&item)));
        assert_eq!(read("src/auth.ts:getUser").unwrap(), ("src/auth.ts:getUser".to_owned(), "function"));
        assert_eq!(read("./src/auth.ts").unwrap(), ("src/auth.ts".to_owned(), "file"));
        assert_eq!(read("src/new.ts").unwrap(), ("src/new.ts".to_owned(), "file"));
        assert_eq!(read("src/billing/").unwrap(), ("src/billing/".to_owned(), "directory"));
        // A directory of the repository, named without its slash.
        assert_eq!(read("src/billing").unwrap(), ("src/billing/".to_owned(), "directory"));
        assert_eq!(read("src/new/").unwrap(), ("src/new/".to_owned(), "directory"));
        assert_eq!(read(".").unwrap(), ("./".to_owned(), "directory"));
        for wrong in ["", "  ", "/etc/passwd", "../other", "src/../../x"] {
            assert!(read(wrong).is_err(), "{wrong:?}");
        }
    }

    #[test]
    fn a_rename_is_told_from_a_change() {
        let renames = vec![("getUser".to_owned(), "fetchUser".to_owned())];
        assert_eq!(
            with_names(b"return getUser(id) + getUserName;", &renames),
            b"return fetchUser(id) + getUserName;"
        );
        assert_eq!(last_word("Session::refresh"), "refresh");
        assert_eq!(last_word("refresh"), "refresh");
    }

    #[test]
    fn what_encloses_a_symbol_covers_it() {
        let class = ScopeItem::Symbol { file: "a.ts".into(), name: "Registry".into(), id: None };
        let other = ScopeItem::Symbol { file: "a.ts".into(), name: "add".into(), id: None };
        let dir = ScopeItem::Dir("src".into());
        let tables = [&SymbolTable::new(), &SymbolTable::new()];
        let a = Path::new("a.ts");
        assert!(covers_symbol(&[&class], tables, None, a, "Registry.add"));
        assert!(covers_symbol(&[&class], tables, None, a, "Registry"));
        assert!(!covers_symbol(&[&class], tables, None, a, "Other.add"));
        assert!(!covers_symbol(&[&class], tables, None, Path::new("b.ts"), "Registry"));
        // A method declared by its plain name.
        assert!(covers_symbol(&[&other], tables, None, a, "Registry.add"));
        assert!(
            covers_file(&[&dir], Path::new("src/x/y.ts")) && !covers_file(&[&dir], Path::new("lib/y.ts"))
        );
        assert!(covers_file(&[&ScopeItem::Dir(PathBuf::new())], Path::new("anything.txt")));
    }
}
