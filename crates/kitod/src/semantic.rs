//! Semantic level 1 in the capture path: the symbolic ops that go with the
//! file-level ops of a batch, the first analysis of files the history only
//! knows as text, and the history of a symbol (`kito why`).

use crate::error::{Error, Result};
use kito_core::{BlobId, FileMode, LaneId, OpKind, SymbolId, SymbolKind, SymbolTable, Tree};
use kito_diff::ast::{diff_symbols, FileChange, SymbolOps, Version};
use kito_diff::text;
use kito_parse::{dialect_for_path, parse, ParsedFile};
use kito_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Files larger than this are tracked as text only: parsing them on every
/// save would cost more than their symbols are worth.
const MAX_PARSED_BYTES: usize = 2 << 20;

/// One version of a file, read from the store and parsed.
struct Side {
    content: Vec<u8>,
    parsed: ParsedFile,
}

fn load(store: &Store, path: &Path, blob: &BlobId) -> Result<Option<Side>> {
    let Some(dialect) = dialect_for_path(path) else { return Ok(None) };
    let Some(content) = store.get_blob(blob)? else { return Ok(None) };
    if content.len() > MAX_PARSED_BYTES {
        return Ok(None);
    }
    Ok(parse(dialect, &content).map(|parsed| Side { content, parsed }))
}

fn version(side: &Option<Side>) -> Option<Version<'_>> {
    side.as_ref().map(|s| Version { content: &s.content, parsed: &s.parsed })
}

fn diff(table: &SymbolTable, lane: LaneId, files: &[(PathBuf, Option<Side>, Option<Side>)]) -> SymbolOps {
    let changes: Vec<FileChange<'_>> = files
        .iter()
        .map(|(path, old, new)| FileChange { path, old: version(old), new: version(new) })
        .collect();
    diff_symbols(table, lane, &changes)
}

/// The symbolic ops for a batch of file-level ops (`tree` and `table` are
/// the state before the batch; the new contents are already in the store).
///
/// A file whose new content has syntax errors is left alone: it was saved
/// half-written, and judging its symbols now would invent removals. Its last
/// content that did parse is remembered in `last_clean`, and that is what
/// the next clean save is compared with.
pub(crate) fn symbol_ops(
    store: &Store,
    tree: &Tree,
    table: &SymbolTable,
    lane: LaneId,
    file_ops: &[OpKind],
    last_clean: &mut HashMap<PathBuf, BlobId>,
) -> Result<SymbolOps> {
    // Old and new blob of every file whose content changed. A file that is
    // removed and added again in the same batch (a change of type, such as
    // `chmod +x`) is one file that changed, not two.
    let mut touched: Vec<(&PathBuf, Option<BlobId>, Option<BlobId>)> = Vec::new();
    let mut slot = |path| match touched.iter().position(|(p, _, _)| *p == path) {
        Some(at) => at,
        None => {
            touched.push((path, None, None));
            touched.len() - 1
        }
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

    let mut files: Vec<(PathBuf, Option<Side>, Option<Side>)> = Vec::new();
    for (path, before, after) in touched {
        if dialect_for_path(path).is_none() {
            continue;
        }
        let new = match after {
            Some(blob) => match load(store, path, &blob)? {
                Some(side) if !side.parsed.has_errors => Some(side),
                _ => {
                    if let Some(before) = before {
                        last_clean.entry(path.clone()).or_insert(before);
                    }
                    continue;
                }
            },
            None => None,
        };
        // The old side is the last content that parsed; if even that has
        // syntax errors, only what the history knows (the names) is left.
        let old = match last_clean.remove(path).or(before) {
            Some(blob) => load(store, path, &blob)?.filter(|side| !side.parsed.has_errors),
            None => None,
        };
        files.push((path.clone(), old, new));
    }
    Ok(diff(table, lane, &files))
}

/// `AddSymbol` ops for the files the history knows only as text: the ones
/// imported from git, or captured before symbols existed.
pub(crate) fn analyze_unindexed(
    store: &Store,
    tree: &Tree,
    table: &SymbolTable,
    lane: LaneId,
) -> Result<SymbolOps> {
    let mut files: Vec<(PathBuf, Option<Side>, Option<Side>)> = Vec::new();
    for (path, entry) in tree.iter() {
        if entry.mode == FileMode::Symlink || table.in_file(path).next().is_some() {
            continue;
        }
        if let Some(side) = load(store, path, &entry.blob)?.filter(|side| !side.parsed.has_errors) {
            files.push((path.to_owned(), None, Some(side)));
        }
    }
    Ok(diff(table, lane, &files))
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

/// The history of the symbol called `name` (qualified or not) in `file`. If
/// several symbols answer to that name (overloads), one history each.
pub(crate) fn why(store: &Store, table: &SymbolTable, file: &Path, name: &str) -> Result<Vec<Why>> {
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
    let mut histories = Vec::new();
    for id in ids {
        let Some(entry) = table.get(id) else { continue };
        let steps = store
            .symbol_ops(id)?
            .iter()
            .filter_map(|op| {
                Some(WhyStep {
                    op: op.id.to_string(),
                    at_ms: op.at.wall_ms,
                    author: op.author.name.clone(),
                    what: describe_symbol_op(store, &op.kind)?,
                    intent: op.intent.and_then(|i| store.intent(&i)).map(|i| i.summary.clone()),
                })
            })
            .collect();
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
