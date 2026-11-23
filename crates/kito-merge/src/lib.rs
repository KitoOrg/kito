//! `kito-merge`: the semantic rebase.
//!
//! Three states of the repository meet here: the `base` a lane started
//! from, what the `main` lane has become since, and what the `lane` has
//! made of it. The result is the main lane's state with the lane's changes
//! applied, composed symbol by symbol:
//!
//! | On the main lane | On the lane | Result |
//! | --- | --- | --- |
//! | change to symbol X | change to symbol Y | both apply |
//! | change to X | change to X | three-way merge of its text; if that fails, `BodyBody` |
//! | rename of X | another rename of X | `RenameRename` |
//! | removal of X | change to X | `RemoveModify` |
//! | move of X | change to X | both apply: the ID follows the symbol |
//! | a new symbol named N | a new one named N, in the same place | once if they are the same text; otherwise `BodyBody`, with no text at the base |
//! | text outside the symbols | the same | three-way merge of the text; if that fails, `Text` |
//!
//! A file is not merged as lines but as what it is made of: a header, its
//! symbols in order, each with the text that leads to it, and a tail; and so
//! is each symbol that contains others. Two sides that add different
//! functions at the same place do not touch the same thing, and a function
//! one side moves to another file while the other edits it is still one
//! function. Only a file that cannot be read this way (no grammar, syntax
//! errors, symbols the history does not know, or symbols of one name that
//! cannot be told apart) is merged as text.
//!
//! Nothing is merged in twice. What both sides added to the same place
//! under the same name is one thing: the same text is in the result once,
//! two texts are a conflict, unless the language lets the two live side by
//! side (overloads with other parameters, a second `impl` block).
//!
//! Where a file's own merge ends in a conflict of kind `Text` and a plain
//! line merge of the file has none (the changes of the two sides neither
//! overlap nor touch), the result is the line merge: this merge may compose
//! more than a line merge does, and less only where the line merge would
//! write a definition twice (one that is neither side's).
//!
//! The crate does no I/O: it is given the three states and a way to read a
//! blob, and returns the files that change and the conflicts found.

use kito_core::{BlobId, ConflictKind, FileMode, SymbolId, SymbolTable, Tree};
use kito_diff::ast::Identities;
use kito_diff::text::{is_binary, merge3, merge3_apart};
use kito_parse::{dialect_for_path, namesakes, parse, Namesakes, ParsedFile};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Files larger than this are merged as text.
const MAX_PARSED_BYTES: usize = 2 << 20;

const BASE: usize = 0;
const MAIN: usize = 1;
const LANE: usize = 2;

/// One of the three states: its files and what the history knows of their symbols.
#[derive(Clone, Copy)]
pub struct Side<'a> {
    pub tree: &'a Tree,
    pub symbols: &'a SymbolTable,
}

/// What a conflict is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Subject {
    Symbol(SymbolId),
    File(PathBuf),
}

/// A decision on a conflict: what the main lane has, what the lane has, or
/// a text written for the purpose (of the symbol, or of the whole file).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Main,
    Lane,
    Text(Vec<u8>),
}

pub struct Input<'a> {
    pub base: Side<'a>,
    pub main: Side<'a>,
    pub lane: Side<'a>,
    /// The content of a blob.
    pub blob: &'a dyn Fn(&BlobId) -> Option<Vec<u8>>,
    /// Decisions already taken: those conflicts are settled as told.
    pub resolutions: &'a HashMap<Subject, Choice>,
    /// Parses a version of a file, given its path, the ID of its content
    /// and the content: for a caller that keeps parsed files around.
    /// `None`: every file is parsed here. The function returns `None` for
    /// a file that is not to be read as a structure.
    pub parse: Option<Parser<'a>>,
}

/// See [`Input::parse`].
pub type Parser<'a> = &'a dyn Fn(&Path, &BlobId, &[u8]) -> Option<ParsedFile>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOut {
    pub content: Vec<u8>,
    pub mode: FileMode,
}

/// A conflict: the two sides cannot both have their way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub kind: ConflictKind,
    pub subject: Subject,
    /// The file it is in.
    pub file: PathBuf,
    /// The symbol's name, for people.
    pub name: Option<String>,
    /// The contended text at the common starting point and on the two
    /// sides. `None`: it does not exist there.
    pub base: Option<Vec<u8>>,
    pub main: Option<Vec<u8>>,
    pub lane: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// The files of the merged state that differ from the main lane's:
    /// their new content, or `None` for a file that goes away. Meaningful
    /// only if there are no conflicts.
    pub files: BTreeMap<PathBuf, Option<FileOut>>,
    /// Which symbol each piece of those files is.
    pub identities: Identities,
    pub conflicts: Vec<Found>,
    /// Symbols both sides changed, whose texts merged on their own.
    pub merged_symbols: Vec<SymbolId>,
    /// Symbols both sides added with the same text, which are in the
    /// result once: each as the main lane has it.
    pub added_by_both: Vec<SymbolId>,
    /// Files whose own merge ended in a conflict of kind `Text` where a
    /// plain line merge had none: they are that line merge.
    pub merged_by_lines: Vec<PathBuf>,
}

/// Where a scope is: a file, or a symbol that contains others.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    File(PathBuf),
    Sym(SymbolId),
}

/// One symbol of a scope with the text around it.
#[derive(Clone, Debug)]
struct Unit {
    sym: usize,
    /// What separates it from the previous unit: blank lines, comments.
    prefix: Range<usize>,
    /// The symbol with the whole of its first and last line: what comes
    /// before it on the line (`export`, the indentation) and the
    /// punctuation that closes it.
    text: Range<usize>,
}

/// A file, or a symbol that contains others, as a sequence of units.
#[derive(Clone, Debug, Default)]
struct Scope {
    header: Range<usize>,
    units: Vec<Unit>,
    tail: Range<usize>,
}

/// One version of one file.
struct View {
    path: PathBuf,
    content: Vec<u8>,
    parsed: Option<ParsedFile>,
    /// `true` if the file can be merged as a structure: it parses cleanly
    /// and every symbol in it is one the history knows.
    structural: bool,
    /// Per parsed symbol, its ID.
    ids: Vec<SymbolId>,
    /// The scope of the file (`None`) and of each symbol that contains others.
    scopes: HashMap<Option<usize>, Scope>,
}

/// Where a symbol is on one side.
#[derive(Clone, Copy, Debug)]
struct At {
    view: usize,
    sym: usize,
}

#[derive(Default)]
struct SideData {
    views: Vec<View>,
    by_path: HashMap<PathBuf, usize>,
    at: HashMap<SymbolId, At>,
}

/// The symbols of a scope in the merged result.
struct Order {
    /// In order. A forced choice comes with the ones a decision brought
    /// back or kept.
    symbols: Vec<(SymbolId, Option<Choice>)>,
    /// The symbols both sides added to the scope under one name: for the
    /// main lane's, which is in the order, the lane's, which is not.
    twins: HashMap<SymbolId, SymbolId>,
    /// The two sides each brought to the scope a thing of the same name,
    /// the two are not one thing added twice (one of them was somewhere
    /// before), and the language does not let them live side by side:
    /// which one stays has to be asked.
    clash: bool,
}

/// What is being written for one file.
#[derive(Default)]
struct Emit {
    bytes: Vec<u8>,
    ids: Vec<(usize, SymbolId)>,
    conflicts: Vec<Found>,
    merged: Vec<SymbolId>,
    /// Symbols both sides added with the same text.
    twice: Vec<SymbolId>,
    symbols: usize,
}

fn line_start(content: &[u8], at: usize) -> usize {
    content[..at].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)
}

fn is_blank(text: &[u8]) -> bool {
    text.iter().all(u8::is_ascii_whitespace)
}

/// Where the text that belongs to the first symbol of a scope begins: the
/// lines right above it, up to a blank line, but nothing of what opens the
/// scope (`class X {`).
fn first_unit_start(
    content: &[u8],
    scope_start: usize,
    body_start: Option<usize>,
    sym_start: usize,
) -> usize {
    let line0 = line_start(content, sym_start).max(scope_start);
    let bound = match body_start {
        None => scope_start,
        Some(body) => {
            let after_opener =
                content[body..].iter().position(|&b| b == b'\n').map_or(content.len(), |i| body + i + 1);
            if line0 >= after_opener {
                after_opener
            } else if body >= line0 && body <= sym_start && is_blank(&content[line0..body]) {
                // The body begins with the symbol itself (Python).
                line0
            } else {
                // On the same line as what opens the scope.
                return sym_start;
            }
        }
    };
    let mut start = line0;
    while start > bound {
        let previous = line_start(content, start - 1);
        if previous < bound || is_blank(&content[previous..start]) {
            break;
        }
        start = previous;
    }
    start
}

/// The end of a symbol's unit: the rest of its last line, if that is only
/// punctuation that closes it.
fn unit_end(content: &[u8], sym_end: usize, limit: usize) -> usize {
    let mut end = sym_end;
    while end < limit && matches!(content[end], b' ' | b'\t' | b';' | b',' | b'\r') {
        end += 1;
    }
    if end < limit && content[end] == b'\n' {
        end + 1
    } else {
        sym_end
    }
}

impl View {
    fn new(
        path: &Path,
        blob: &BlobId,
        content: Vec<u8>,
        table: &SymbolTable,
        parser: Option<Parser<'_>>,
    ) -> View {
        let mut view = View {
            path: path.to_owned(),
            content,
            parsed: None,
            structural: false,
            ids: Vec::new(),
            scopes: HashMap::new(),
        };
        let Some(dialect) = dialect_for_path(path) else { return view };
        if view.content.len() > MAX_PARSED_BYTES || is_binary(&view.content) {
            return view;
        }
        let parsed = match parser {
            Some(parser) => parser(path, blob, &view.content),
            None => parse(dialect, &view.content),
        };
        let Some(parsed) = parsed else { return view };

        // The symbols the history knows in this file, recognized as the
        // differ does.
        let ids = kito_diff::ast::identify(table, path, &parsed);
        let expected = table.in_file(path).count();
        let mut structural = !parsed.has_errors && ids.iter().all(Option::is_some) && ids.len() == expected;

        // Scopes: the file, and every symbol that contains others.
        let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        children.entry(None).or_default();
        for (i, sym) in parsed.symbols.iter().enumerate() {
            children.entry(sym.parent).or_default().push(i);
        }
        for (container, members) in children {
            let (range, body) = match container {
                None => (0..view.content.len(), None),
                Some(c) => (parsed.symbols[c].range.clone(), Some(parsed.symbols[c].header.end)),
            };
            let mut scope = Scope { header: range.clone(), units: Vec::new(), tail: range.end..range.end };
            let mut previous_end = range.start;
            for (n, &m) in members.iter().enumerate() {
                let sym = &parsed.symbols[m].range;
                let limit = members.get(n + 1).map_or(range.end, |&next| parsed.symbols[next].range.start);
                if sym.start < previous_end || sym.end > limit {
                    structural = false; // two symbols on the same text
                    break;
                }
                let prefix_start = match n {
                    0 => first_unit_start(&view.content, range.start, body, sym.start),
                    _ => previous_end,
                };
                if n == 0 {
                    scope.header = range.start..prefix_start;
                }
                let end = unit_end(&view.content, sym.end, limit);
                let text_start = line_start(&view.content, sym.start).max(prefix_start);
                scope.units.push(Unit { sym: m, prefix: prefix_start..text_start, text: text_start..end });
                previous_end = end;
            }
            if !scope.units.is_empty() {
                scope.tail = previous_end..range.end;
            }
            view.scopes.insert(container, scope);
        }
        view.structural = structural;
        if structural {
            view.ids = ids.into_iter().flatten().collect();
        }
        view.parsed = Some(parsed);
        view
    }

    fn parent_of(&self, sym: usize) -> Option<usize> {
        self.parsed.as_ref().and_then(|p| p.symbols[sym].parent)
    }

    fn range_of(&self, sym: usize) -> Range<usize> {
        self.parsed.as_ref().map_or(0..0, |p| p.symbols[sym].range.clone())
    }

    /// The unit of a symbol in the scope that contains it.
    fn unit_of(&self, sym: usize) -> Option<&Unit> {
        self.scopes.get(&self.parent_of(sym))?.units.iter().find(|u| u.sym == sym)
    }

    /// The symbols strictly inside `sym`.
    fn inside(&self, sym: usize) -> impl Iterator<Item = usize> + '_ {
        let outer = self.range_of(sym);
        let count = self.parsed.as_ref().map_or(0, |p| p.symbols.len());
        (sym + 1..count).take_while(move |&j| self.range_of(j).start < outer.end)
    }
}

impl SideData {
    fn view(&self, path: &Path) -> Option<&View> {
        self.by_path.get(path).map(|&i| &self.views[i])
    }

    fn scope_key(&self, id: SymbolId) -> Option<Key> {
        let at = self.at.get(&id)?;
        let view = &self.views[at.view];
        Some(match view.parent_of(at.sym) {
            Some(parent) => Key::Sym(view.ids[parent]),
            None => Key::File(view.path.clone()),
        })
    }

    /// The text of a symbol's unit.
    fn unit_text(&self, id: SymbolId) -> Option<&[u8]> {
        let (view, unit) = self.unit(id)?;
        Some(&view.content[unit.text.clone()])
    }

    /// What precedes the symbol on its first line.
    fn lead(&self, id: SymbolId) -> Option<&[u8]> {
        let (view, unit) = self.unit(id)?;
        Some(&view.content[unit.text.start..view.range_of(unit.sym).start])
    }

    fn unit(&self, id: SymbolId) -> Option<(&View, &Unit)> {
        let at = self.at.get(&id)?;
        let view = &self.views[at.view];
        Some((view, view.unit_of(at.sym)?))
    }

    /// What the parser says of a symbol.
    fn parsed(&self, id: SymbolId) -> Option<(&ParsedFile, &kito_parse::ParsedSymbol)> {
        let at = self.at.get(&id)?;
        let file = self.views[at.view].parsed.as_ref()?;
        Some((file, &file.symbols[at.sym]))
    }

    /// From now on a file is text: nothing in it is taken for a symbol.
    fn as_text(&mut self, path: &Path) {
        let Some(&index) = self.by_path.get(path) else { return };
        self.views[index].structural = false;
        self.views[index].ids.clear();
        self.at.retain(|_, at| at.view != index);
    }
}

/// White space aside, the same text.
fn same_but_for_space(a: &[u8], b: &[u8]) -> bool {
    let squeeze = |t: &[u8]| t.iter().copied().filter(|c| !c.is_ascii_whitespace()).collect::<Vec<u8>>();
    squeeze(a) == squeeze(b)
}

/// Three-way merge of a piece of text that may not exist on a side.
fn merge_piece(base: Option<&[u8]>, main: Option<&[u8]>, lane: Option<&[u8]>) -> Option<Vec<u8>> {
    match (main, lane) {
        (None, None) => Some(Vec::new()),
        (Some(only), None) | (None, Some(only)) => Some(only.to_vec()),
        (Some(main), Some(lane)) => {
            if main == lane || base == Some(lane) {
                return Some(main.to_vec());
            }
            if base == Some(main) {
                return Some(lane.to_vec());
            }
            // How many blank lines separate two things is not worth a conflict.
            if same_but_for_space(main, lane) {
                return Some(main.to_vec());
            }
            merge3(base?, main, lane)
        }
    }
}

/// Replaces every whole-word occurrence of `from`.
fn rename_word(text: &[u8], from: &str, to: &str) -> Vec<u8> {
    let word = |c: Option<&u8>| c.is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$');
    let from = from.as_bytes();
    let mut out = Vec::with_capacity(text.len() + 16);
    let mut i = 0;
    while i < text.len() {
        let hit = text[i..].starts_with(from)
            && !word(i.checked_sub(1).and_then(|p| text.get(p)))
            && !word(text.get(i + from.len()));
        if hit {
            out.extend_from_slice(to.as_bytes());
            i += from.len();
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    out
}

/// Length of the blank lines a text starts with.
fn leading_blank_lines(text: &[u8]) -> usize {
    let mut end = 0;
    for line in text.split_inclusive(|&c| c == b'\n') {
        if !line.ends_with(b"\n") || !is_blank(line) {
            break;
        }
        end += line.len();
    }
    end
}

/// The white space the first line of a text starts with.
fn indent_of(text: &[u8]) -> Vec<u8> {
    text.iter().copied().take_while(|c| matches!(c, b' ' | b'\t')).collect()
}

/// Removes `indent` from the start of every line that has it.
fn dedent(text: &[u8], indent: &[u8]) -> Vec<u8> {
    text.split_inclusive(|&c| c == b'\n')
        .flat_map(|line| line.strip_prefix(indent).unwrap_or(line))
        .copied()
        .collect()
}

/// Adds `indent` to the start of every line that is not empty.
fn indent(text: &[u8], indent: &[u8]) -> Vec<u8> {
    text.split_inclusive(|&c| c == b'\n')
        .flat_map(|line| {
            let blank = line.iter().all(u8::is_ascii_whitespace);
            indent.iter().filter(move |_| !blank).chain(line)
        })
        .copied()
        .collect()
}

fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
}

struct Merger<'a> {
    input: &'a Input<'a>,
    sides: [SideData; 3],
    /// Per side (main, lane): the symbols of the base it changed, moved or removed.
    changed: [HashSet<SymbolId>; 3],
    /// Files whose symbols of one name cannot be told apart, and that can
    /// be merged neither as symbols nor as text: see [`merge`].
    unsure: HashSet<PathBuf>,
    out: Outcome,
}

/// Merges what the lane did since `base` into the main lane.
pub fn merge(input: &Input<'_>) -> Outcome {
    let trees = [input.base.tree, input.main.tree, input.lane.tree];
    let tables = [input.base.symbols, input.main.symbols, input.lane.symbols];
    let mut merger = Merger {
        input,
        sides: Default::default(),
        changed: Default::default(),
        unsure: HashSet::new(),
        out: Outcome::default(),
    };
    let by_lane: BTreeSet<PathBuf> = trees[BASE].differences(trees[LANE]).map(Path::to_owned).collect();
    if by_lane.is_empty() && input.resolutions.is_empty() {
        return merger.out;
    }
    // A lane that stayed behind sees many files that only the main lane
    // changed. They are as the main lane has them and are not even read,
    // except the ones tied to the lane's work: a symbol went from one of
    // them to a file the lane touched, or the other way round; or the main
    // lane gave a new name to a file the lane touched.
    let renamed_on_main = by_lane.iter().any(|p| trees[BASE].contains(p) && !trees[MAIN].contains(p));
    let mut touched = by_lane.clone();
    for path in trees[BASE].differences(trees[MAIN]) {
        if by_lane.contains(path) {
            continue;
        }
        let comes_from_the_lanes_files = |from: usize, to: usize| {
            tables[from]
                .in_file(path)
                .any(|(id, _)| tables[to].get(id).is_some_and(|entry| by_lane.contains(&entry.file)))
        };
        if comes_from_the_lanes_files(MAIN, BASE)
            || comes_from_the_lanes_files(BASE, MAIN)
            || (renamed_on_main && !trees[BASE].contains(path))
        {
            touched.insert(path.to_owned());
        }
    }

    // The three versions of every file involved.
    for side in [BASE, MAIN, LANE] {
        for path in &touched {
            let Some(entry) = trees[side].get(path) else { continue };
            let content = (input.blob)(&entry.blob).unwrap_or_default();
            let view = View::new(path, &entry.blob, content, tables[side], input.parse);
            let data = &mut merger.sides[side];
            if view.structural {
                for (sym, id) in view.ids.iter().enumerate() {
                    data.at.insert(*id, At { view: data.views.len(), sym });
                }
            }
            data.by_path.insert(path.clone(), data.views.len());
            data.views.push(view);
        }
    }
    // A file where symbols of one name do not line up with the base's is
    // not read as a structure: on no side, so that the three agree. If one
    // of its symbols is in another file on a side, its text is not all
    // there is to merge either (a change to that symbol must follow it):
    // the file is then asked about, whatever the rest composes to.
    let astray: Vec<&PathBuf> = touched.iter().filter(|path| merger.namesakes_astray(path)).collect();
    for path in astray {
        if !merger.stays_within(path) {
            merger.unsure.insert(path.clone());
            continue;
        }
        for side in [BASE, MAIN, LANE] {
            merger.sides[side].as_text(path);
        }
    }
    for side in [MAIN, LANE] {
        let changed: HashSet<SymbolId> = merger.sides[BASE]
            .at
            .keys()
            .copied()
            .filter(|&id| {
                !merger.sides[side].at.contains_key(&id)
                    || merger.sides[side].scope_key(id) != merger.sides[BASE].scope_key(id)
                    || merger.sides[side].unit_text(id) != merger.sides[BASE].unit_text(id)
            })
            .collect();
        merger.changed[side] = changed;
    }

    let mut settled = merger.renames(&touched);
    for path in &touched {
        if settled.insert(path.clone()) {
            merger.file(path);
        }
    }
    merger.out
}

impl Merger<'_> {
    fn tree(&self, side: usize) -> &Tree {
        [self.input.base.tree, self.input.main.tree, self.input.lane.tree][side]
    }

    fn table(&self, side: usize) -> &SymbolTable {
        [self.input.base.symbols, self.input.main.symbols, self.input.lane.symbols][side]
    }

    fn content(&self, side: usize, path: &Path) -> Option<Vec<u8>> {
        let entry = self.tree(side).get(path)?;
        match self.sides[side].view(path) {
            Some(view) => Some(view.content.clone()),
            None => (self.input.blob)(&entry.blob),
        }
    }

    /// Records the merged content of `path`, if it is not what the main lane has.
    fn output(&mut self, path: &Path, file: Option<FileOut>) {
        let current = self.tree(MAIN).get(path);
        let same = match (&file, current) {
            (None, None) => true,
            (Some(file), Some(entry)) => entry.blob == BlobId::of(&file.content) && entry.mode == file.mode,
            _ => false,
        };
        if !same {
            self.out.files.insert(path.to_owned(), file);
        }
    }

    /// The identities of a file taken whole from one side, under the path
    /// it gets in the result.
    fn identities_of(&mut self, side: usize, from: &Path, to: &Path) {
        let Some(view) = self.sides[side].view(from).filter(|v| v.structural) else { return };
        for (sym, id) in view.ids.iter().enumerate() {
            self.out.identities.insert(to, view.range_of(sym).start, *id);
        }
    }

    fn text_conflict(&mut self, path: &Path) {
        let found = Found {
            kind: ConflictKind::Text,
            subject: Subject::File(path.to_owned()),
            file: path.to_owned(),
            name: None,
            base: self.content(BASE, path),
            main: self.content(MAIN, path),
            lane: self.content(LANE, path),
        };
        self.out.conflicts.push(found);
    }

    /// Files one side renamed without changing them: the other side's
    /// version follows under the new name. Returns the paths settled here.
    fn renames(&mut self, touched: &BTreeSet<PathBuf>) -> HashSet<PathBuf> {
        let mut settled = HashSet::new();
        for (renamer, other) in [(LANE, MAIN), (MAIN, LANE)] {
            let candidates: Vec<&PathBuf> =
                touched.iter().filter(|p| !self.tree(BASE).contains(p) && !settled.contains(*p)).collect();
            for to in candidates {
                let Some(new) = self.tree(renamer).get(to).copied() else { continue };
                if self.tree(other).contains(to) {
                    continue;
                }
                let from = touched.iter().find(|from| {
                    !settled.contains(*from)
                        && !self.tree(renamer).contains(from)
                        && self.tree(BASE).get(from).is_some_and(|old| old.blob == new.blob)
                });
                let Some(from) = from.cloned() else { continue };
                let base = self.tree(BASE).get(&from).copied();
                let theirs = self.tree(other).get(&from).copied();
                settled.insert(from.clone());
                settled.insert(to.clone());
                let decided = self.input.resolutions.get(&Subject::File(to.clone())).cloned();
                match (theirs, decided) {
                    // The other side left the file alone: the rename is all there is.
                    (Some(theirs), None) if Some(theirs) == base => {
                        if renamer == LANE {
                            let content = self.content(LANE, to).unwrap_or_default();
                            self.identities_of(LANE, to, to);
                            self.output(to, Some(FileOut { content, mode: new.mode }));
                            self.output(&from, None);
                        }
                    }
                    // The other side changed it: its version, under the new name.
                    (Some(_), None) => {
                        let content = self.content(other, &from).unwrap_or_default();
                        self.identities_of(other, &from, to);
                        self.output(to, Some(FileOut { content, mode: new.mode }));
                        self.output(&from, None);
                    }
                    // Renamed here, removed there.
                    (None, None) => self.text_conflict(to),
                    (_, Some(choice)) => {
                        let keep_new = matches!(
                            (&choice, renamer),
                            (Choice::Text(_), _) | (Choice::Main, MAIN) | (Choice::Lane, LANE)
                        );
                        let file = keep_new.then(|| FileOut {
                            content: match choice {
                                Choice::Text(text) => text,
                                _ => self.content(renamer, to).unwrap_or_default(),
                            },
                            mode: new.mode,
                        });
                        if keep_new {
                            self.identities_of(renamer, to, to);
                        }
                        self.output(to, file);
                        self.output(&from, None);
                    }
                }
            }
        }
        settled
    }

    /// `true` if the file, as one side has it, contains a symbol the other
    /// side changed somewhere else: it cannot be taken as it is.
    fn crossed(&self, side: usize, path: &Path, other: usize) -> bool {
        self.sides[side]
            .view(path)
            .is_some_and(|view| view.structural && view.ids.iter().any(|id| self.changed[other].contains(id)))
    }

    /// Merges one file.
    fn file(&mut self, path: &Path) {
        let [base, main, lane] = [BASE, MAIN, LANE].map(|side| self.tree(side).get(path).copied());
        let mode = match (main, lane) {
            // The lane changed the mode: its choice.
            (Some(_), Some(l)) if Some(l.mode) != base.map(|b| b.mode) => l.mode,
            (Some(m), _) => m.mode,
            (None, Some(l)) => l.mode,
            (None, None) => FileMode::Regular,
        };
        if let Some(choice) = self.input.resolutions.get(&Subject::File(path.to_owned())).cloned() {
            match choice {
                Choice::Main => {}
                Choice::Lane => {
                    let file = lane.map(|l| FileOut {
                        content: self.content(LANE, path).unwrap_or_default(),
                        mode: l.mode,
                    });
                    self.identities_of(LANE, path, path);
                    self.output(path, file);
                }
                Choice::Text(content) => self.output(path, Some(FileOut { content, mode })),
            }
            return;
        }
        let structural = [BASE, MAIN, LANE].iter().all(|&side| match self.sides[side].view(path) {
            Some(view) => view.structural,
            None => true,
        }) && dialect_for_path(path).is_some();
        if main == lane {
            // The same file on both sides, unless its bytes are other
            // symbols on each and one of them is a symbol the other side
            // changed where it stood: a function one side moved here, where
            // the other wrote a copy of it and changed the function. The
            // change has to reach it: the file is merged as symbols.
            let made_of = |side: usize| self.sides[side].view(path).map(|view| view.ids.clone());
            let crossed = structural
                && main.is_some()
                && made_of(MAIN) != made_of(LANE)
                && (self.crossed(MAIN, path, LANE) || self.crossed(LANE, path, MAIN));
            if crossed {
                self.structure(path, mode);
            }
            return;
        }

        if lane == base {
            // Only the main lane touched the file: it stays as it is there,
            // unless it now holds a symbol the lane changed elsewhere.
            if structural && self.crossed(MAIN, path, LANE) {
                self.structure(path, mode);
            }
        } else if main == base {
            if structural && self.crossed(LANE, path, MAIN) {
                self.structure(path, mode);
            } else {
                let file = lane
                    .map(|l| FileOut { content: self.content(LANE, path).unwrap_or_default(), mode: l.mode });
                self.identities_of(LANE, path, path);
                self.output(path, file);
            }
        } else if structural && (base.is_some() || main.is_none() || lane.is_none()) {
            self.structure(path, mode);
        } else {
            // As text.
            let merged = match (main, lane, base) {
                (Some(_), Some(_), Some(_)) => {
                    let texts = [BASE, MAIN, LANE].map(|side| self.content(side, path).unwrap_or_default());
                    merge3(&texts[BASE], &texts[MAIN], &texts[LANE])
                }
                _ => None,
            };
            match merged {
                Some(content) => self.output(path, Some(FileOut { content, mode })),
                None if self.by_lines(path, mode, false) => {}
                None => self.text_conflict(path),
            }
        }
    }

    /// `true` if, in a file, symbols of one name are not on a side the
    /// ones they are at the base. The history tells namesakes apart by the
    /// order they were created in, a file by the order they are written
    /// in: when a side writes a new overload above an old one the two
    /// orders part, and what the base knows as the old one is taken for the
    /// new one. A symbol that is not what the base has under its ID, while
    /// a namesake of it is, says so.
    fn namesakes_astray(&self, path: &Path) -> bool {
        let Some(base) = self.sides[BASE].view(path).filter(|view| view.structural) else { return false };
        let Some(parsed) = &base.parsed else { return false };
        let was: HashMap<SymbolId, (kito_core::Hash, kito_core::Hash)> =
            base.ids.iter().zip(&parsed.symbols).map(|(id, sym)| (*id, (sym.signature, sym.body))).collect();
        [MAIN, LANE].iter().any(|&side| {
            let Some(view) = self.sides[side].view(path).filter(|view| view.structural) else { return false };
            let Some(parsed) = &view.parsed else { return false };
            let symbols = &parsed.symbols;
            let mut groups: HashMap<(Option<SymbolId>, kito_core::SymbolKind, &str), Vec<usize>> =
                HashMap::new();
            for (i, sym) in symbols.iter().enumerate() {
                let key = (sym.parent.map(|parent| view.ids[parent]), sym.kind, sym.name.as_str());
                groups.entry(key).or_default().push(i);
            }
            groups.values().filter(|group| group.len() > 1).any(|group| {
                group.iter().any(|&i| {
                    let Some(&(signature, body)) = was.get(&view.ids[i]) else { return false };
                    let is = |j: usize| (symbols[j].signature, symbols[j].body);
                    // It changed, and another one is as the base has it, or
                    // at least has its parameters.
                    is(i) != (signature, body)
                        && group.iter().any(|&j| {
                            j != i
                                && (is(j) == (signature, body)
                                    || (symbols[j].signature == signature
                                        && symbols[i].signature != signature))
                        })
                })
            })
        })
    }

    /// `true` if no symbol of the file is, on one of the three sides, in
    /// another file: nothing in it came from elsewhere or went there, and
    /// its own text is all there is to merge.
    fn stays_within(&self, path: &Path) -> bool {
        let sides = [BASE, MAIN, LANE];
        sides.iter().all(|&side| {
            self.table(side).in_file(path).all(|(id, _)| {
                sides.iter().all(|&other| self.table(other).get(id).is_none_or(|entry| entry.file == path))
            })
        })
    }

    /// Settles a file whose own merge ended in a conflict of kind `Text`
    /// with a plain line merge of it, if that has no conflict: what a line
    /// merge composes beyond doubt is not asked about. `false` if it has
    /// one too, or if a side does not have the file.
    ///
    /// `as_symbols`: the file was read as symbols on the three sides. Its
    /// line merge is then taken only if it can be read so too, and every
    /// definition in it is one a side has. Lines do not know that a
    /// function both sides added, or both moved, to two different places
    /// is one function: they keep it twice, and the second is neither
    /// side's, or is the symbol the first already is (moved and renamed by
    /// one side, moved elsewhere by the other, it is there under its two
    /// names). Such a result is not what the two sides made, and the
    /// conflict stands.
    fn by_lines(&mut self, path: &Path, mode: FileMode, as_symbols: bool) -> bool {
        let [Some(base), Some(main), Some(lane)] = [BASE, MAIN, LANE].map(|side| self.content(side, path))
        else {
            return false;
        };
        let Some(merged) = merge3_apart(&base, &main, &lane) else { return false };
        // Which symbol each definition of the result is: the one the main
        // lane has in that place and by that name, else the lane's.
        let parsed = dialect_for_path(path)
            .filter(|_| merged.len() <= MAX_PARSED_BYTES)
            .and_then(|dialect| parse(dialect, &merged))
            .filter(|parsed| !parsed.has_errors);
        let mut ids: Vec<(usize, SymbolId)> = Vec::new();
        let mut taken: HashSet<SymbolId> = HashSet::new();
        match &parsed {
            Some(parsed) => {
                let known = [MAIN, LANE].map(|side| kito_diff::ast::identify(self.table(side), path, parsed));
                for (i, sym) in parsed.symbols.iter().enumerate() {
                    match known[0][i].or(known[1][i]) {
                        // A symbol is one definition, never two.
                        Some(id) if taken.insert(id) => ids.push((sym.range.start, id)),
                        _ if as_symbols => return false,
                        _ => {}
                    }
                }
            }
            None if as_symbols => return false,
            None => {}
        }
        for (offset, id) in ids {
            self.out.identities.insert(path, offset, id);
        }
        self.out.merged_by_lines.push(path.to_owned());
        self.output(path, Some(FileOut { content: merged, mode }));
        true
    }

    /// Merges one file as a structure.
    fn structure(&mut self, path: &Path, mode: FileMode) {
        if self.unsure.contains(path) {
            return self.text_conflict(path);
        }
        let scopes =
            [BASE, MAIN, LANE].map(|side| self.sides[side].by_path.get(path).map(|&view| (view, None)));
        let mut emit = Emit::default();
        self.scope(&Key::File(path.to_owned()), scopes, path, &mut emit);
        if !emit.conflicts.is_empty() {
            // Text between the symbols that the structure cannot compose
            // and lines can: a function added after statements one of
            // which the other side changed. Only where the text is all
            // that stands in the way, and all the file is made of: a
            // symbol that went to another file takes its changes along,
            // which lines do not know.
            let only_text = emit.conflicts.iter().all(|conflict| conflict.kind == ConflictKind::Text);
            if !(only_text && self.stays_within(path) && self.by_lines(path, mode, true)) {
                self.out.conflicts.append(&mut emit.conflicts);
            }
            return;
        }
        let present = |side: usize| self.tree(side).contains(path);
        if present(BASE) && (!present(MAIN) || !present(LANE)) {
            // Removed on one side. If nothing the other side did is left,
            // the removal stands; otherwise the two do not go together.
            let survivor = if present(MAIN) { MAIN } else { LANE };
            let untouched_around = match (self.sides[BASE].view(path), self.sides[survivor].view(path)) {
                (Some(base), Some(kept)) => {
                    let around = |v: &View| {
                        let scope = &v.scopes[&None];
                        (v.content[scope.header.clone()].to_vec(), v.content[scope.tail.clone()].to_vec())
                    };
                    around(base) == around(kept)
                }
                _ => false,
            };
            if emit.symbols == 0 && untouched_around {
                self.output(path, None);
            } else {
                self.text_conflict(path);
            }
            return;
        }
        for (offset, id) in emit.ids {
            self.out.identities.insert(path, offset, id);
        }
        self.out.merged_symbols.append(&mut emit.merged);
        self.out.added_by_both.append(&mut emit.twice);
        self.output(path, Some(FileOut { content: emit.bytes, mode }));
    }

    fn name(&self, side: usize, id: SymbolId) -> Option<String> {
        self.table(side).get(id).map(|entry| entry.name.clone())
    }

    fn symbol_conflict(&self, kind: ConflictKind, id: SymbolId, path: &Path) -> Found {
        let text = |side: usize| {
            self.sides[side].unit(id).map(|(view, unit)| view.content[unit.text.clone()].to_vec())
        };
        Found {
            kind,
            subject: Subject::Symbol(id),
            file: path.to_owned(),
            name: [MAIN, LANE, BASE]
                .iter()
                .find_map(|&side| self.table(side).qualified_name(id))
                .or_else(|| self.name(BASE, id)),
            base: text(BASE),
            main: text(MAIN),
            lane: text(LANE),
        }
    }

    /// `true` if a symbol the main lane added to a scope and one the lane
    /// added there are one thing added twice: the same kind and name, and
    /// either the same text or nothing in the language that lets the two
    /// live side by side.
    fn added_twice(&self, main: SymbolId, lane: SymbolId) -> bool {
        let (Some(ours), Some(theirs)) = (self.table(MAIN).get(main), self.table(LANE).get(lane)) else {
            return false;
        };
        if ours.kind != theirs.kind || ours.name != theirs.name {
            return false;
        }
        if self.same_addition(main, lane) {
            return true;
        }
        match namesakes(theirs.lang, theirs.kind, &theirs.name) {
            Namesakes::Never => true,
            Namesakes::Always => false,
            // The same parameters: the same overload.
            Namesakes::Overloads => {
                let signature =
                    |side: usize, id: SymbolId| self.sides[side].parsed(id).map(|(_, sym)| sym.signature);
                signature(MAIN, main) == signature(LANE, lane)
            }
        }
    }

    /// `true` if a symbol of the main lane and one of the lane have the
    /// same kind, the same name and the same text.
    fn same_addition(&self, main: SymbolId, lane: SymbolId) -> bool {
        let (Some(ours), Some(theirs)) = (self.table(MAIN).get(main), self.table(LANE).get(lane)) else {
            return false;
        };
        let texts = (self.sides[MAIN].unit_text(main), self.sides[LANE].unit_text(lane));
        ours.kind == theirs.kind && ours.name == theirs.name && matches!(texts, (Some(a), Some(b)) if a == b)
    }

    /// The symbols of a scope in the merged result, in order.
    fn sequence(&self, seqs: [&[SymbolId]; 3], path: &Path, emit: &mut Emit) -> Order {
        let [base, main, lane] = seqs;
        let in_base: HashSet<SymbolId> = base.iter().copied().collect();
        let exists = |side: usize, id: SymbolId| self.table(side).get(id).is_some();
        let decided = |id: SymbolId| self.input.resolutions.get(&Subject::Symbol(id)).cloned();
        let mut merged: Vec<(SymbolId, Option<Choice>)> = Vec::new();
        let mut twins: HashMap<SymbolId, SymbolId> = HashMap::new();
        let mut clash = false;

        // The main lane's order, minus what the lane took away.
        for &id in main {
            let gone_from_lane = !lane.contains(&id);
            let was_here = in_base.contains(&id);
            if was_here && gone_from_lane && exists(LANE, id) {
                continue; // the lane moved it elsewhere: it is merged there
            }
            let removed_by_lane =
                gone_from_lane && !exists(LANE, id) && self.sides[BASE].at.contains_key(&id);
            if removed_by_lane {
                // Removed on the lane. If the main lane changed or moved it
                // in the meantime, the two do not go together.
                if !self.changed[MAIN].contains(&id) {
                    continue;
                }
                match decided(id) {
                    Some(Choice::Lane) => continue,
                    Some(choice) => merged.push((id, Some(choice))),
                    None => {
                        emit.conflicts.push(self.symbol_conflict(ConflictKind::RemoveModify, id, path));
                        merged.push((id, None));
                    }
                }
                continue;
            }
            merged.push((id, None));
        }

        // What the lane added to this scope, each after the sibling it
        // follows on the lane.
        let mut anchor: Option<SymbolId> = None;
        for &id in lane {
            if merged.iter().any(|(m, _)| *m == id) {
                anchor = Some(id);
                continue;
            }
            let mut forced = None;
            if in_base.contains(&id) {
                if exists(MAIN, id) {
                    continue; // the main lane moved it elsewhere: it stays there
                }
                // Removed on the main lane.
                if !self.changed[LANE].contains(&id) {
                    continue;
                }
                match decided(id) {
                    Some(Choice::Main) => continue,
                    Some(choice) => forced = Some(choice),
                    None => {
                        emit.conflicts.push(self.symbol_conflict(ConflictKind::RemoveModify, id, path));
                        continue;
                    }
                }
            } else if self.sides[BASE].at.contains_key(&id) {
                // Moved here by the lane.
                if !exists(MAIN, id) {
                    match decided(id) {
                        Some(Choice::Main) => continue,
                        Some(choice) => forced = Some(choice),
                        None => {
                            emit.conflicts.push(self.symbol_conflict(ConflictKind::RemoveModify, id, path));
                            continue;
                        }
                    }
                } else if self.sides[MAIN].scope_key(id) != self.sides[BASE].scope_key(id) {
                    continue; // the main lane moved it too: its place wins
                }
            } else if exists(MAIN, id) {
                continue; // the main lane has it somewhere else
            }
            // The main lane added one of the same name to this scope: one
            // thing added twice, not two to keep side by side. What follows
            // it on the lane follows the main lane's. Only what is new on
            // both sides is one thing: a symbol the base has, which a side
            // moved here, is itself wherever it is, and what the other side
            // did to it has to reach it. Of several of the name, the one
            // with the same text before another.
            let twin = match forced.is_none() && !exists(BASE, id) {
                false => None,
                true => {
                    let mine = || {
                        merged.iter().map(|(m, _)| *m).filter(|m| {
                            !in_base.contains(m)
                                && !exists(BASE, *m)
                                && !lane.contains(m)
                                && !twins.contains_key(m)
                        })
                    };
                    mine()
                        .find(|m| self.same_addition(*m, id))
                        .or_else(|| mine().find(|m| self.added_twice(*m, id)))
                }
            };
            if let Some(twin) = twin {
                twins.insert(twin, id);
                anchor = Some(twin);
                continue;
            }
            // The same name as something only the main lane has here, and
            // not its twin: a method the lane moved in where the main lane
            // wrote a copy of it, or the other way round. Both are written,
            // so that nothing either side did is lost, and the file is asked
            // about: the two cannot both stay.
            clash |= merged
                .iter()
                .any(|(m, _)| !lane.contains(m) && !twins.contains_key(m) && self.added_twice(*m, id));
            // After its anchor, and among what the main lane added at the
            // same place in an order that does not depend on who came first.
            let mut at = match anchor {
                Some(anchor) => merged.iter().position(|(m, _)| *m == anchor).map_or(0, |i| i + 1),
                None => 0,
            };
            while at < merged.len() && !in_base.contains(&merged[at].0) && merged[at].0 < id {
                at += 1;
            }
            merged.insert(at, (id, forced));
            anchor = Some(id);
        }
        Order { symbols: merged, twins, clash }
    }

    /// Merges a scope: its header, its symbols in the merged order, its tail.
    fn scope(&self, key: &Key, at: [Option<(usize, Option<usize>)>; 3], path: &Path, emit: &mut Emit) {
        let scopes = [BASE, MAIN, LANE].map(|side| {
            at[side].and_then(|(view, container)| {
                let view = &self.sides[side].views[view];
                view.scopes.get(&container).map(|scope| (view, scope))
            })
        });
        let piece = |side: usize, range: fn(&Scope) -> Range<usize>| -> Option<&[u8]> {
            scopes[side].map(|(view, scope)| &view.content[range(scope)])
        };
        let ids = |side: usize| -> Vec<SymbolId> {
            scopes[side]
                .map_or(Vec::new(), |(view, scope)| scope.units.iter().map(|u| view.ids[u.sym]).collect())
        };
        let seqs = [ids(BASE), ids(MAIN), ids(LANE)];
        let mut conflict_here = false;

        let header = |s: &Scope| s.header.clone();
        match merge_piece(piece(BASE, header), piece(MAIN, header), piece(LANE, header)) {
            Some(text) => emit.bytes.extend_from_slice(&text),
            None => conflict_here = true,
        }
        let Order { symbols, twins, clash } =
            self.sequence([&seqs[BASE], &seqs[MAIN], &seqs[LANE]], path, emit);
        conflict_here |= clash;
        for (n, (id, forced)) in symbols.into_iter().enumerate() {
            // What the lane added under the same name: one thing with it.
            let twin = twins.get(&id).copied();
            let prefix = |side: usize| {
                let id = twin.filter(|_| side == LANE).unwrap_or(id);
                self.sides[side].unit(id).map(|(view, unit)| &view.content[unit.prefix.clone()])
            };
            match merge_piece(prefix(BASE), prefix(MAIN), prefix(LANE)) {
                Some(text) => {
                    // A symbol that opens the scope only because each side
                    // removed something before it does not keep the blank
                    // lines that separated it from what is gone: the result
                    // must not depend on who removed first.
                    let opens_by_accident =
                        n == 0 && seqs[MAIN].first() != Some(&id) && seqs[LANE].first() != Some(&id);
                    let skip = match opens_by_accident {
                        true => leading_blank_lines(&text),
                        false => 0,
                    };
                    emit.bytes.extend_from_slice(&text[skip..]);
                }
                None => conflict_here = true,
            }
            emit.symbols += 1;
            match twin {
                Some(twin) => self.added_by_both(id, twin, path, emit),
                None => self.symbol(id, forced, path, emit),
            }
        }
        let tail = |s: &Scope| s.tail.clone();
        match merge_piece(piece(BASE, tail), piece(MAIN, tail), piece(LANE, tail)) {
            Some(text) => emit.bytes.extend_from_slice(&text),
            None => conflict_here = true,
        }
        if conflict_here {
            // Text outside the symbols that both sides changed: the file as
            // a whole is what must be decided.
            let _ = key;
            let file = Subject::File(path.to_owned());
            if !emit.conflicts.iter().any(|c| c.subject == file) {
                emit.conflicts.push(Found {
                    kind: ConflictKind::Text,
                    subject: file,
                    file: path.to_owned(),
                    name: None,
                    base: self.content(BASE, path),
                    main: self.content(MAIN, path),
                    lane: self.content(LANE, path),
                });
            }
        }
    }

    /// Copies a symbol as one side has it, with everything it contains.
    fn whole(&self, side: usize, id: SymbolId, emit: &mut Emit) {
        let Some((view, unit)) = self.sides[side].unit(id) else { return };
        let start = emit.bytes.len();
        for sym in std::iter::once(unit.sym).chain(view.inside(unit.sym)) {
            emit.ids.push((start + view.range_of(sym).start - unit.text.start, view.ids[sym]));
        }
        emit.bytes.extend_from_slice(&view.content[unit.text.clone()]);
    }

    /// `true` if the symbol, as `side` has it, contains one the other side
    /// changed: it must be merged member by member.
    fn crossed_inside(&self, side: usize, id: SymbolId, other: usize) -> bool {
        self.sides[side].at.get(&id).is_some_and(|at| {
            let view = &self.sides[side].views[at.view];
            view.inside(at.sym).any(|inner| self.changed[other].contains(&view.ids[inner]))
        })
    }

    /// The symbols inside `id` as a side has it, in order.
    fn inside_ids(&self, side: usize, id: SymbolId) -> Vec<SymbolId> {
        self.sides[side].at.get(&id).map_or(Vec::new(), |at| {
            let view = &self.sides[side].views[at.view];
            view.inside(at.sym).map(|inner| view.ids[inner]).collect()
        })
    }

    /// Merges a symbol member by member.
    fn members(&self, id: SymbolId, path: &Path, emit: &mut Emit) {
        let lead = |side: usize| self.sides[side].lead(id);
        let text = merge_piece(lead(BASE), lead(MAIN), lead(LANE))
            .or_else(|| lead(MAIN).map(<[u8]>::to_vec))
            .unwrap_or_default();
        emit.bytes.extend_from_slice(&text);
        emit.ids.push((emit.bytes.len(), id));
        let at =
            [BASE, MAIN, LANE].map(|side| self.sides[side].at.get(&id).map(|at| (at.view, Some(at.sym))));
        self.scope(&Key::Sym(id), at, path, emit);
        // What follows the symbol on its last line.
        let trailer = |side: usize| {
            self.sides[side]
                .unit(id)
                .map(|(view, unit)| &view.content[view.range_of(unit.sym).end..unit.text.end])
        };
        let text = merge_piece(trailer(BASE), trailer(MAIN), trailer(LANE))
            .or_else(|| trailer(MAIN).map(<[u8]>::to_vec))
            .unwrap_or_default();
        emit.bytes.extend_from_slice(&text);
    }

    /// Notes that the text about to be written is the symbol `id`. Where
    /// on its first line the definition starts depends on what precedes it
    /// (`export`): every place it starts at on a side is a candidate.
    fn know(&self, id: SymbolId, emit: &mut Emit) {
        let start = emit.bytes.len();
        emit.ids.push((start, id));
        for side in [BASE, MAIN, LANE] {
            if let Some(lead) = self.sides[side].lead(id) {
                emit.ids.push((start + lead.len(), id));
            }
        }
    }

    /// Writes a symbol as one side has it: whole, unless it holds a symbol
    /// the other side changed (a new class a method was moved into).
    fn one_sided(&self, side: usize, other: usize, id: SymbolId, path: &Path, emit: &mut Emit) {
        match self.crossed_inside(side, id, other) {
            true => self.members(id, path, emit),
            false => self.whole(side, id, emit),
        }
    }

    /// Writes a symbol both sides added to the same scope under the same
    /// name (`main` there, `lane` here): it is one thing. With the same
    /// text on both sides it is written once; with two texts the sides do
    /// not go together, and the conflict is the main lane's symbol with
    /// nothing at the base.
    fn added_by_both(&self, main: SymbolId, lane: SymbolId, path: &Path, emit: &mut Emit) {
        match self.input.resolutions.get(&Subject::Symbol(main)).cloned() {
            Some(Choice::Main) => return self.one_sided(MAIN, LANE, main, path, emit),
            Some(Choice::Lane) => return self.one_sided(LANE, MAIN, lane, path, emit),
            Some(Choice::Text(mut text)) => {
                if !text.ends_with(b"\n") {
                    text.push(b'\n');
                }
                self.know(main, emit);
                emit.bytes.extend_from_slice(&text);
                return;
            }
            None => {}
        }
        let text = |side: usize, id: SymbolId| {
            self.sides[side].unit(id).map(|(view, unit)| view.content[unit.text.clone()].to_vec())
        };
        let (ours, theirs) = (text(MAIN, main), text(LANE, lane));
        match ours == theirs {
            true => emit.twice.push(main),
            false => emit.conflicts.push(Found {
                kind: ConflictKind::BodyBody,
                subject: Subject::Symbol(main),
                file: path.to_owned(),
                name: self.table(MAIN).qualified_name(main).or_else(|| self.name(MAIN, main)),
                base: None,
                main: ours,
                lane: theirs,
            }),
        }
        self.one_sided(MAIN, LANE, main, path, emit);
    }

    /// Writes the merged text of one symbol.
    fn symbol(&self, id: SymbolId, forced: Option<Choice>, path: &Path, emit: &mut Emit) {
        let has = |side: usize| self.sides[side].at.contains_key(&id);
        let decided = forced.or_else(|| self.input.resolutions.get(&Subject::Symbol(id)).cloned());
        match decided {
            Some(Choice::Main) if has(MAIN) => return self.whole(MAIN, id, emit),
            Some(Choice::Lane) if has(LANE) => return self.whole(LANE, id, emit),
            Some(Choice::Text(mut text)) => {
                if !text.ends_with(b"\n") {
                    text.push(b'\n');
                }
                self.know(id, emit);
                emit.bytes.extend_from_slice(&text);
                return;
            }
            _ => {}
        }
        // On one side only: as it is there.
        match (has(MAIN), has(LANE)) {
            (true, false) => return self.one_sided(MAIN, LANE, id, path, emit),
            (false, true) => return self.one_sided(LANE, MAIN, id, path, emit),
            (false, false) => return,
            (true, true) => {}
        }
        let text = |side: usize| self.sides[side].unit_text(id);
        let changed_on_main = text(MAIN) != text(BASE);
        let changed_on_lane = text(LANE) != text(BASE);
        if !changed_on_lane {
            return match self.crossed_inside(MAIN, id, LANE) {
                true => self.members(id, path, emit),
                false => self.whole(MAIN, id, emit),
            };
        }
        if !changed_on_main {
            return match self.crossed_inside(LANE, id, MAIN) {
                true => self.members(id, path, emit),
                false => self.whole(LANE, id, emit),
            };
        }
        if text(MAIN) == text(LANE) {
            // The same text is the same thing, unless it is made of other
            // symbols on the two sides and one of them is a symbol the other
            // side changed where it stood (a method one side moved in, where
            // the other wrote a copy of it and changed the method): member
            // by member then, so that the change is not lost with a text
            // taken whole.
            let crossed = self.inside_ids(MAIN, id) != self.inside_ids(LANE, id)
                && (self.crossed_inside(MAIN, id, LANE) || self.crossed_inside(LANE, id, MAIN));
            return match crossed {
                true => self.members(id, path, emit),
                false => self.whole(MAIN, id, emit),
            };
        }

        // Both sides changed it.
        let names = [BASE, MAIN, LANE].map(|side| self.name(side, id));
        let renamed = |side: usize| names[BASE].is_some() && names[side] != names[BASE];
        if renamed(MAIN) && renamed(LANE) && names[MAIN] != names[LANE] {
            emit.conflicts.push(self.symbol_conflict(ConflictKind::RenameRename, id, path));
            return self.whole(MAIN, id, emit);
        }
        let contains_others = [BASE, MAIN, LANE].iter().any(|&side| {
            self.sides[side].at.get(&id).is_some_and(|at| {
                self.sides[side].views[at.view].scopes.get(&Some(at.sym)).is_some_and(|s| !s.units.is_empty())
            })
        });
        if contains_others {
            return self.members(id, path, emit);
        }

        // A symbol with nothing inside: its text, line by line. A rename on
        // one side is first carried over to the other two texts, so that
        // the new name does not fight with whatever else changed.
        let unit = |side: usize| {
            self.sides[side].unit(id).map(|(view, unit)| view.content[unit.text.clone()].to_vec())
        };
        let (Some(mut base), Some(mut main), Some(mut lane)) = (unit(BASE), unit(MAIN), unit(LANE)) else {
            emit.conflicts.push(self.symbol_conflict(ConflictKind::BodyBody, id, path));
            return self.whole(MAIN, id, emit);
        };
        for (side, other) in [(MAIN, &mut lane), (LANE, &mut main)] {
            if let (true, Some(old), Some(new)) = (renamed(side), &names[BASE], &names[side]) {
                if is_plain_name(old) && is_plain_name(new) {
                    base = rename_word(&base, old, new);
                    *other = rename_word(other, old, new);
                }
            }
        }
        // A symbol one side moved to a place with another indentation
        // changed on every line: the three texts are compared without their
        // indentation, and the result gets the one of where the symbol goes.
        let indents = [indent_of(&base), indent_of(&main), indent_of(&lane)];
        let moved_by_lane = self.sides[LANE].scope_key(id) != self.sides[BASE].scope_key(id);
        let target = if moved_by_lane { indents[LANE].clone() } else { indents[MAIN].clone() };
        let reindent = indents.iter().any(|i| *i != target);
        if reindent {
            base = dedent(&base, &indents[BASE]);
            main = dedent(&main, &indents[MAIN]);
            lane = dedent(&lane, &indents[LANE]);
        }
        match merge3(&base, &main, &lane) {
            Some(mut merged) => {
                if reindent {
                    merged = indent(&merged, &target);
                }
                self.know(id, emit);
                emit.bytes.extend_from_slice(&merged);
                emit.merged.push(id);
            }
            None => {
                emit.conflicts.push(self.symbol_conflict(ConflictKind::BodyBody, id, path));
                self.whole(MAIN, id, emit);
            }
        }
    }
}
