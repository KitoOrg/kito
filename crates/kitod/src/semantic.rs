//! Semantic level 1 in the capture path: the symbolic ops that go with the
//! file-level ops of a batch, the first analysis of files the history only
//! knows as text, and the history of a symbol (`kito why`).

use crate::error::{Error, Result};
use kito_core::{BlobId, FileMode, Hlc, LaneId, Op, OpKind, SymbolId, SymbolKind, SymbolTable, Tree};
use kito_diff::ast::{diff_symbols_with, FileChange, Identities, SymbolOps, Version};
use kito_diff::text;
use kito_parse::{dialect_for_path, parse_reusing, Parsed};
use kito_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Files larger than this are tracked as text only: parsing them on every
/// save would cost more than their symbols are worth.
const MAX_PARSED_BYTES: usize = 2 << 20;

/// One version of a file, read from the store and parsed.
type Side = Arc<Parsed>;

/// Parsed versions of the files captured most recently, by content. The old
/// side of a save is almost always here (it was the new side of the save
/// before), and its syntax tree lets the new side be parsed incrementally.
#[derive(Default)]
pub(crate) struct ParseCache {
    entries: HashMap<BlobId, (Side, u64)>,
    bytes: usize,
    clock: u64,
}

impl ParseCache {
    const MAX_ENTRIES: usize = 1024;
    const MAX_BYTES: usize = 16 << 20;

    fn get(&mut self, blob: &BlobId) -> Option<Side> {
        self.clock += 1;
        let (side, used) = self.entries.get_mut(blob)?;
        *used = self.clock;
        Some(side.clone())
    }

    fn insert(&mut self, blob: BlobId, side: Side) {
        self.clock += 1;
        self.bytes += side.source().len();
        if let Some((old, _)) = self.entries.insert(blob, (side, self.clock)) {
            self.bytes -= old.source().len();
        }
        // The least recently used go first.
        while self.entries.len() > Self::MAX_ENTRIES || self.bytes > Self::MAX_BYTES {
            let Some(oldest) = self.entries.iter().min_by_key(|(_, (_, used))| *used).map(|(blob, _)| *blob)
            else {
                break;
            };
            if let Some((gone, _)) = self.entries.remove(&oldest) {
                self.bytes -= gone.source().len();
            }
        }
    }
}

/// Reads a version of a file from the store and parses it, reusing the tree
/// of `previous` if given. `None`: not a language we parse, or too big.
pub(crate) fn load(
    store: &Store,
    cache: Option<&mut ParseCache>,
    path: &Path,
    blob: &BlobId,
    previous: Option<&Parsed>,
) -> Result<Option<Side>> {
    let Some(dialect) = dialect_for_path(path) else { return Ok(None) };
    let mut cache = cache;
    if let Some(hit) = cache.as_mut().and_then(|c| c.get(blob)) {
        return Ok(Some(hit));
    }
    let Some(content) = store.get_blob(blob)? else { return Ok(None) };
    if content.len() > MAX_PARSED_BYTES {
        return Ok(None);
    }
    let Some(parsed) = parse_reusing(dialect, &content, previous) else { return Ok(None) };
    let side = Arc::new(parsed);
    if let Some(cache) = cache {
        cache.insert(*blob, side.clone());
    }
    Ok(Some(side))
}

fn version(side: &Option<Side>) -> Option<Version<'_>> {
    side.as_ref().map(|s| Version { content: s.source(), parsed: &s.file })
}

fn diff(
    table: &SymbolTable,
    lane: LaneId,
    files: &[(PathBuf, Option<Side>, Option<Side>)],
    identities: &Identities,
) -> SymbolOps {
    let changes: Vec<FileChange<'_>> = files
        .iter()
        .map(|(path, old, new)| FileChange { path, old: version(old), new: version(new) })
        .collect();
    diff_symbols_with(table, lane, &changes, identities)
}

/// The symbolic ops for a batch of file-level ops (`tree` and `table` are
/// the state before the batch; the new contents are already in the store).
///
/// A file whose new content has syntax errors is left alone: it was saved
/// half-written, and judging its symbols now would invent removals. Its last
/// content that did parse is remembered in `last_clean`, and that is what
/// the next clean save is compared with.
///
/// `identities`: symbols of the new contents known in advance (the result
/// of a merge, an explicit rename).
#[allow(clippy::too_many_arguments)]
pub(crate) fn symbol_ops(
    store: &Store,
    tree: &Tree,
    table: &SymbolTable,
    lane: LaneId,
    file_ops: &[OpKind],
    last_clean: &mut HashMap<PathBuf, BlobId>,
    cache: &mut ParseCache,
    identities: &Identities,
) -> Result<SymbolOps> {
    // Old and new blob of every file whose content changed. A file that is
    // removed and added again in the same batch (a change of type, such as
    // `chmod +x`) is one file that changed, not two.
    let mut touched: Vec<(&PathBuf, Option<BlobId>, Option<BlobId>)> = Vec::new();
    let mut slots: HashMap<&PathBuf, usize> = HashMap::new();
    let mut slot = |path| {
        *slots.entry(path).or_insert_with(|| {
            touched.push((path, None, None));
            touched.len() - 1
        })
    };
    let mut edits: Vec<(usize, Option<BlobId>, Option<BlobId>)> = Vec::new();
    for op in file_ops {
        match op {
            OpKind::TextEdit { file, before, after, .. } => {
                edits.push((slot(file), Some(*before), Some(*after)))
            }
            OpKind::FileAdd { path, blob, mode } if *mode != FileMode::Symlink => {
                edits.push((slot(path), None, Some(*blob)))
            }
            OpKind::FileRemove { path } => {
                edits.push((slot(path), tree.get(path).map(|entry| entry.blob), None))
            }
            // A renamed file keeps its content: its symbols just follow it.
            _ => {}
        }
    }
    for (at, before, after) in edits {
        touched[at].1 = touched[at].1.or(before);
        touched[at].2 = touched[at].2.or(after);
    }

    // Many new files and nothing they could be confused with: an import, the
    // first capture of a project. No symbol can have moved between them.
    let only_new = touched.len() > BULK
        && identities.knows_nothing()
        && touched.iter().all(|(path, before, after)| {
            before.is_none() && after.is_some() && !last_clean.contains_key(*path)
        });
    if only_new {
        return new_files(store, table, lane, &touched, identities);
    }

    let mut files: Vec<(PathBuf, Option<Side>, Option<Side>)> = Vec::new();
    for (path, before, after) in touched {
        if dialect_for_path(path).is_none() {
            continue;
        }
        // The old side is the last content that parsed; if even that has
        // syntax errors, only what the history knows (the names) is left.
        let old_blob = last_clean.get(path).copied().or(before);
        let old = match old_blob {
            Some(blob) => load(store, Some(cache), path, &blob, None)?,
            None => None,
        };
        let new = match after {
            Some(blob) => match load(store, Some(cache), path, &blob, old.as_deref())? {
                Some(side) if !side.file.has_errors => Some(side),
                _ => {
                    if let Some(before) = before {
                        last_clean.entry(path.clone()).or_insert(before);
                    }
                    continue;
                }
            },
            None => None,
        };
        last_clean.remove(path);
        files.push((path.clone(), old.filter(|side| !side.file.has_errors), new));
    }
    Ok(diff(table, lane, &files, identities))
}

/// More new files than this in one batch are read a group at a time.
const BULK: usize = 256;

/// The symbols of many new files: parsed on every processor, a group of
/// files at a time so that the syntax trees of a whole repository are never
/// in memory together. The ops are the ones the files would give one by one.
fn new_files(
    store: &Store,
    table: &SymbolTable,
    lane: LaneId,
    touched: &[(&PathBuf, Option<BlobId>, Option<BlobId>)],
    identities: &Identities,
) -> Result<SymbolOps> {
    let mut all = SymbolOps::default();
    let mut identities = identities.clone();
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).min(16);
    for group in touched.chunks(BULK) {
        let mut contents: Vec<(&PathBuf, kito_parse::Dialect, Vec<u8>)> = Vec::new();
        for (path, _, after) in group {
            let (Some(dialect), Some(blob)) = (dialect_for_path(path), after) else { continue };
            match store.get_blob(blob)? {
                Some(content) if content.len() <= MAX_PARSED_BYTES => contents.push((path, dialect, content)),
                _ => {}
            }
        }
        let share = contents.len().div_ceil(workers).max(1);
        let parsed: Vec<Option<Parsed>> = std::thread::scope(|scope| {
            let handles: Vec<_> = contents
                .chunks(share)
                .map(|part| {
                    scope.spawn(move || {
                        part.iter()
                            .map(|(_, dialect, content)| parse_reusing(*dialect, content, None))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles.into_iter().flat_map(|handle| handle.join().expect("a parser thread panicked")).collect()
        });
        let files: Vec<(PathBuf, Option<Side>, Option<Side>)> = contents
            .iter()
            .zip(parsed)
            .filter_map(|((path, _, _), parsed)| {
                // Half-written code is left alone, as in a single save.
                let parsed = parsed.filter(|parsed| !parsed.file.has_errors)?;
                Some(((*path).clone(), None, Some(Arc::new(parsed))))
            })
            .collect();
        let found = diff(table, lane, &files, &identities);
        // The next group continues the numbering of this one.
        let last = found.ops.iter().filter_map(|op| match op {
            OpKind::AddSymbol { sym, .. } if sym.id.origin() == lane => Some(sym.id.seq()),
            _ => None,
        });
        if let Some(last) = last.max() {
            identities.start_new_ids_at(last + 1);
        }
        all.ops.extend(found.ops);
        all.blobs.extend(found.blobs);
    }
    Ok(all)
}

/// `AddSymbol` ops for the files the history knows only as text: the ones
/// imported from git, or captured before symbols existed.
pub(crate) fn analyze_unindexed(
    store: &Store,
    tree: &Tree,
    table: &SymbolTable,
    lane: LaneId,
) -> Result<SymbolOps> {
    let paths: Vec<(PathBuf, BlobId)> = tree
        .iter()
        .filter(|(path, entry)| entry.mode != FileMode::Symlink && table.in_file(path).next().is_none())
        .map(|(path, entry)| (path.to_owned(), entry.blob))
        .collect();
    let files: Vec<(&PathBuf, Option<BlobId>, Option<BlobId>)> =
        paths.iter().map(|(path, blob)| (path, None, Some(*blob))).collect();
    new_files(store, table, lane, &files, &Identities::new())
}

/// One event in the life of a symbol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhyStep {
    /// ID of the op.
    pub op: String,
    pub at_ms: u64,
    pub author: String,
    /// What happened, in words.
    pub what: String,
    pub intent: Option<String>,
}

/// The history of one symbol, across renames and moves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Why {
    /// The symbol's permanent ID.
    pub id: String,
    pub kind: String,
    /// Current qualified name and file.
    pub name: String,
    pub file: String,
    /// Oldest first.
    pub steps: Vec<WhyStep>,
}

pub(crate) fn kind_word(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "module",
        SymbolKind::Class => "class",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Interface => "interface",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Field => "field",
        SymbolKind::Const => "constant",
        SymbolKind::Var => "variable",
        SymbolKind::TypeAlias => "type alias",
        SymbolKind::Block => "block",
    }
}

/// What a symbolic op did, in words.
pub(crate) fn describe_symbol_op(store: &Store, kind: &OpKind) -> Option<String> {
    Some(match kind {
        OpKind::AddSymbol { sym, .. } => {
            format!("{} `{}` created in {}", kind_word(sym.kind), sym.name, sym.file.display())
        }
        OpKind::RemoveSymbol { .. } => "removed".to_owned(),
        OpKind::Rename { from, to, .. } => format!("renamed `{from}` → `{to}`"),
        // Same file: the symbol changed enclosing symbol (a method that went
        // to another class).
        OpKind::Move { from, to, parent, .. } if from.file == to.file => {
            match parent.and_then(|container| last_known_name(store, container)) {
                Some(container) => format!("moved into `{container}` in {}", to.file.display()),
                None => format!("moved to the top level of {}", to.file.display()),
            }
        }
        OpKind::Move { from, to, .. } => {
            format!("moved from {} to {}", from.file.display(), to.file.display())
        }
        OpKind::ModifySignature { .. } => "signature changed".to_owned(),
        OpKind::ModifyBody { before, edits, .. } => {
            if edits.is_empty() {
                "body changed".to_owned()
            } else {
                let old = store.get_blob(before).ok().flatten().unwrap_or_default();
                let stat = text::stat(&old, edits);
                format!("body changed (+{} −{})", stat.lines_added, stat.lines_removed)
            }
        }
        _ => return None,
    })
}

/// The name a symbol had after the last op that named it: for symbols that
/// are no longer in the table.
pub(crate) fn last_known_name(store: &Store, id: SymbolId) -> Option<String> {
    let mut name = None;
    for op in store.symbol_ops(id).ok()? {
        match op.kind {
            OpKind::AddSymbol { sym, .. } => name = Some(sym.name),
            OpKind::Rename { to, .. } => name = Some(to),
            _ => {}
        }
    }
    name
}

/// The ops that tell the story of a symbol, oldest first: its own, and the
/// moves between files of the symbols that contained it at the time (a
/// method follows its class without a `Move` of its own). With each
/// inherited op, the container it happened to.
pub(crate) fn timeline(store: &Store, id: SymbolId, depth: usize) -> Result<Vec<(Op, Option<SymbolId>)>> {
    let own = store.symbol_ops(id)?;
    // Under which container the symbol was, and when.
    let mut periods: Vec<(SymbolId, Hlc, Option<Hlc>)> = Vec::new();
    let mut current: Option<(SymbolId, Hlc)> = None;
    for op in &own {
        let parent = match &op.kind {
            OpKind::AddSymbol { sym, .. } => sym.parent,
            OpKind::Move { parent, .. } => *parent,
            _ => continue,
        };
        if let Some((container, since)) = current.take() {
            periods.push((container, since, Some(op.at)));
        }
        current = parent.map(|container| (container, op.at));
    }
    periods.extend(current.map(|(container, since)| (container, since, None)));

    let mut events: Vec<(Op, Option<SymbolId>)> = Vec::new();
    // Nesting is never this deep; the bound only guards against a damaged log.
    if depth < 32 {
        for (container, since, until) in periods {
            for (op, via) in timeline(store, container, depth + 1)? {
                let during = op.at > since && until.is_none_or(|until| op.at < until);
                let to_another_file =
                    matches!(&op.kind, OpKind::Move { from, to, .. } if from.file != to.file);
                if during && to_another_file {
                    events.push((op, Some(via.unwrap_or(container))));
                }
            }
        }
    }
    events.extend(own.into_iter().map(|op| (op, None)));
    events.sort_by_key(|(op, _)| op.at);
    Ok(events)
}

/// Which ops are part of the history of a lane: all of its own, and of
/// every other lane those recorded before the point where the two last met.
pub(crate) struct Reach {
    lane: LaneId,
    /// Per other lane: the time of the last of its ops this lane has.
    until: HashMap<LaneId, Hlc>,
}

impl Reach {
    pub(crate) fn of(store: &Store, lane: LaneId) -> Result<Reach> {
        let at = |id: &kito_core::OpId| -> Result<Option<Hlc>> { Ok(store.get_op(id)?.map(|op| op.at)) };
        let mut until = HashMap::new();
        for record in store.lane_records() {
            if record.id != lane && record.since > 0 {
                // The ops of another lane that were integrated.
                if let Some(last) = store.lane_ops(record.id, record.since - 1, 1)?.first() {
                    until.insert(record.id, last.at);
                }
            }
        }
        // What its parent had when this lane last started from it, what
        // the parent's parent had then, and so on down to the main lane
        // (ADR 0001).
        let mut below = lane;
        for _ in 0..64 {
            let Some(record) = store.lane_record(below).filter(|_| !below.is_main()) else { break };
            if let Some(time) = at(&record.base_head)? {
                until.insert(record.parent_lane(), time);
            }
            below = record.parent_lane();
        }
        Ok(Reach { lane, until })
    }

    pub(crate) fn includes(&self, op: &Op) -> bool {
        op.lane == self.lane || self.until.get(&op.lane).is_some_and(|until| op.at <= *until)
    }
}

/// The renames of files recorded in a lane, oldest first.
fn file_renames(store: &Store, lane: LaneId) -> Result<Vec<Op>> {
    const PAGE: usize = 4096;
    let mut renames = Vec::new();
    let mut start = 0;
    loop {
        let ops = store.lane_ops(lane, start, PAGE)?;
        let read = ops.len();
        renames.extend(ops.into_iter().filter(|op| matches!(op.kind, OpKind::FileRename { .. })));
        if read < PAGE {
            return Ok(renames);
        }
        start += read as u64;
    }
}

/// The history of the symbol called `name` (qualified or not) in `file`. If
/// several symbols answer to that name (overloads), one history each.
pub(crate) fn why(
    store: &Store,
    table: &SymbolTable,
    lane: LaneId,
    file: &Path,
    name: &str,
) -> Result<Vec<Why>> {
    let ids = table.find(file, name);
    if ids.is_empty() {
        let mut known: Vec<String> =
            table.in_file(file).filter_map(|(id, _)| table.qualified_name(id)).collect();
        known.sort();
        known.dedup();
        return Err(Error::Other(if known.is_empty() {
            format!("{}: no symbols are known in this file", file.display())
        } else {
            format!("{}: no symbol named `{name}`; the file contains: {}", file.display(), known.join(", "))
        }));
    }
    let reach = Reach::of(store, lane)?;
    let mut histories = Vec::new();
    for id in ids {
        let Some(entry) = table.get(id) else { continue };
        // The story as this lane knows it: its own ops, and of the other
        // lanes only what has reached it.
        let events: Vec<(Op, Option<SymbolId>)> =
            timeline(store, id, 0)?.into_iter().filter(|(op, _)| reach.includes(op)).collect();

        // Where the symbol's own ops leave it. If that is not where it is
        // now, its file was renamed along the way: the log of the lane says
        // when. (Reading it all is the price of the rare case.)
        let lands = events.iter().rev().find_map(|(op, _)| match &op.kind {
            OpKind::AddSymbol { sym, .. } => Some(&sym.file),
            OpKind::Move { to, .. } => Some(&to.file),
            _ => None,
        });
        let renames = if lands == Some(&entry.file) { Vec::new() } else { file_renames(store, lane)? };
        let mut renames = renames.into_iter().peekable();

        let step = |op: &Op, what: String| WhyStep {
            op: op.id.to_string(),
            at_ms: op.at.wall_ms,
            author: op.author.name.clone(),
            what,
            intent: op.intent.and_then(|i| store.intent(&i)).map(|i| i.summary.clone()),
        };
        let mut steps = Vec::new();
        let (mut born, mut gone) = (false, false);
        let mut at_file: Option<PathBuf> = None;
        let mut follow_renames = |before: Option<Hlc>,
                                  at_file: &mut Option<PathBuf>,
                                  steps: &mut Vec<WhyStep>| {
            while let Some(op) = renames.next_if(|op| before.is_none_or(|before| op.at < before)) {
                let OpKind::FileRename { from, to } = &op.kind else { continue };
                if at_file.as_ref() == Some(from) {
                    steps
                        .push(step(&op, format!("file renamed from {} to {}", from.display(), to.display())));
                    *at_file = Some(to.clone());
                }
            }
        };
        for (op, via) in &events {
            follow_renames(Some(op.at), &mut at_file, &mut steps);
            match &op.kind {
                OpKind::AddSymbol { sym, .. } => at_file = Some(sym.file.clone()),
                OpKind::Move { to, .. } => at_file = Some(to.file.clone()),
                _ => {}
            }
            let Some(mut what) = describe_symbol_op(store, &op.kind) else { continue };
            if let Some(container) = via {
                let container =
                    table.qualified_name(*container).or_else(|| last_known_name(store, *container));
                what = format!("{what}, along with `{}`", container.unwrap_or_default());
            }
            // An `AddSymbol` for a symbol that already has a story is not a
            // creation: the symbol is back after a removal, or it arrives
            // here from the lane it was born on.
            let added = matches!(op.kind, OpKind::AddSymbol { .. }) && via.is_none();
            let removed = matches!(op.kind, OpKind::RemoveSymbol { .. }) && via.is_none();
            if added && born && gone {
                if let OpKind::AddSymbol { sym, .. } = &op.kind {
                    what = format!("restored in {}", sym.file.display());
                }
            } else if added && born && op.lane == lane {
                what = match lane.is_main() {
                    true => "integrated into the main lane".to_owned(),
                    false => format!("arrived on {lane}"),
                };
            }
            born |= added;
            gone = (gone || removed) && !added;
            if op.lane != lane {
                what = format!("{what}, on {}", op.lane);
            }
            steps.push(step(op, what));
        }
        follow_renames(None, &mut at_file, &mut steps);

        histories.push(Why {
            id: id.to_string(),
            kind: kind_word(entry.kind).to_owned(),
            name: table.qualified_name(id).unwrap_or_else(|| entry.name.clone()),
            file: entry.file.display().to_string(),
            steps,
        });
    }
    Ok(histories)
}
