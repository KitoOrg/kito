//! Measures symbol identity on real code: how often a renamed or moved
//! symbol keeps its ID, and how often two different symbols are mistaken for
//! one.
//!
//! ```text
//! scripts/fetch-corpus.sh
//! cargo run --release -p kito-diff --example corpus -- target/corpus
//! ```
//!
//! The corpus is one open-source repository per language. Git history does
//! not say which functions were renamed, so the ground truth is made here:
//! every eligible symbol of every file is, in turn,
//!
//! * **renamed** (every whole-word occurrence of its name in the file, as a
//!   refactoring tool would do) — it must keep its ID;
//! * **renamed and edited** in the same save (one identifier of its body
//!   changes too) — reported apart: it is no longer a pure rename, and a
//!   symbol too small to have a fingerprint is not expected to survive it;
//! * **moved** to another file of the corpus, if it is a top-level symbol or
//!   sits in a namespace — it must keep its ID;
//! * **replaced** by a different symbol taken from another file — the
//!   newcomer must get a new ID.
//!
//! A mutation that does not leave syntactically valid code is discarded.

use kito_core::{LaneId, SymbolId, SymbolKind, SymbolTable};
use kito_diff::ast::{diff_symbols, FileChange, Version};
use kito_parse::{dialect_for_path, parse, Dialect, ParsedFile};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 200_000;
const MAX_FILES: usize = 400;
/// Symbols tried per file and per kind of mutation.
const PER_FILE: usize = 6;

struct Source {
    path: PathBuf,
    text: String,
    parsed: ParsedFile,
}

#[derive(Default)]
struct Tally {
    tried: u32,
    passed: u32,
}

impl Tally {
    fn record(&mut self, ok: bool) {
        self.tried += 1;
        self.passed += u32::from(ok);
    }

    fn rate(&self) -> String {
        if self.tried == 0 {
            return "—".to_owned();
        }
        format!(
            "{:.1}% ({}/{})",
            100.0 * f64::from(self.passed) / f64::from(self.tried),
            self.passed,
            self.tried
        )
    }
}

#[derive(Default)]
struct Report {
    files: usize,
    symbols: usize,
    renames: Tally,
    /// Renames of symbols too small to be told apart by their body alone.
    small_renames: Tally,
    /// Renamed and edited at once, with enough text to have a fingerprint...
    edited_renames: Tally,
    /// ...and without.
    small_edited_renames: Tally,
    moves: Tally,
    /// Moves of symbols that were inside a namespace.
    nested_moves: Tally,
    /// "Passed" here means: correctly NOT taken for the same symbol.
    replacements: Tally,
}

fn is_identifier(name: &str) -> bool {
    name.len() >= 3
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Replaces every whole-word occurrence of `name`.
fn rename_in(text: &str, name: &str, to: &str) -> String {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
    let mut out = String::with_capacity(text.len() + 64);
    let mut rest = text;
    while let Some(at) = rest.find(name) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + name.len()..].chars().next();
        out.push_str(&rest[..at]);
        out.push_str(if word(before) || word(after) { name } else { to });
        rest = &rest[at + name.len()..];
    }
    out.push_str(rest);
    out
}

/// A small edit inside `range`: the last identifier other than `name` gets
/// a suffix. `None` if there is none.
fn edit_in(text: &str, range: std::ops::Range<usize>, name: &str) -> Option<String> {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let body = &text[range.clone()];
    let mut end = body.len();
    while end > 0 {
        // The last run of word characters before `end`.
        let stop = body[..end].rfind(word)? + 1;
        let start = body[..stop].rfind(|c: char| !word(c)).map_or(0, |at| at + 1);
        let found = &body[start..stop];
        if found.len() >= 3 && found != name && found.starts_with(|c: char| c.is_ascii_alphabetic()) {
            let at = range.start + stop;
            return Some(format!("{}Edited{}", &text[..at], &text[at..]));
        }
        end = start;
    }
    None
}

fn collect(dir: &Path, dialects: &[Dialect], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" || name == "vendor" || name == "target" {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect(&path, dialects, out),
            Ok(kind) if kind.is_file() => {
                let small = entry.metadata().is_ok_and(|m| m.len() <= MAX_FILE_BYTES);
                if small && dialect_for_path(&path).is_some_and(|d| dialects.contains(&d)) {
                    out.push(path);
                }
            }
            _ => {}
        }
    }
}

/// The table after `file` was recorded for the first time, and the ID each
/// of its symbols got.
fn recorded(sources: &[&Source]) -> (SymbolTable, HashMap<(PathBuf, usize), SymbolId>) {
    let mut table = SymbolTable::new();
    let changes: Vec<FileChange<'_>> = sources
        .iter()
        .map(|s| FileChange {
            path: &s.path,
            old: None,
            new: Some(Version { content: s.text.as_bytes(), parsed: &s.parsed }),
        })
        .collect();
    for op in diff_symbols(&table, LaneId::MAIN, &changes).ops {
        table.apply(&op).expect("ops apply");
    }
    let mut ids = HashMap::new();
    for source in sources {
        for index in 0..source.parsed.symbols.len() {
            let name = source.parsed.qualified_name(index);
            // Namesakes are told apart by order, as the differ does.
            let nth = (0..index).filter(|&j| source.parsed.qualified_name(j) == name).count();
            if let Some(id) = table.find(&source.path, &name).get(nth) {
                ids.insert((source.path.clone(), index), *id);
            }
        }
    }
    (table, ids)
}

/// Applies new contents (`None`: unchanged) and returns the table afterwards.
fn step(table: &SymbolTable, edits: &[(&Source, &str)]) -> Option<SymbolTable> {
    let mut parsed = Vec::new();
    for (source, text) in edits {
        let file = parse(dialect_for_path(&source.path)?, text.as_bytes())?;
        if file.has_errors {
            return None; // the mutation broke the code: not a fair test
        }
        parsed.push(file);
    }
    let changes: Vec<FileChange<'_>> = edits
        .iter()
        .zip(&parsed)
        .map(|((source, text), new)| FileChange {
            path: &source.path,
            old: Some(Version { content: source.text.as_bytes(), parsed: &source.parsed }),
            new: Some(Version { content: text.as_bytes(), parsed: new }),
        })
        .collect();
    let mut table = table.clone();
    for op in diff_symbols(&table, LaneId::MAIN, &changes).ops {
        table.apply(&op).ok()?;
    }
    Some(table)
}

fn measure(sources: &[Source]) -> Report {
    let mut report = Report { files: sources.len(), ..Report::default() };
    for (at, source) in sources.iter().enumerate() {
        report.symbols += source.parsed.symbols.len();
        let other = &sources[(at + 1) % sources.len()];
        let (table, ids) = recorded(&[source, other]);

        // Eligible: a plain identifier that names exactly one symbol of the file.
        let eligible: Vec<usize> = (0..source.parsed.symbols.len())
            .filter(|&i| {
                let sym = &source.parsed.symbols[i];
                is_identifier(&sym.name)
                    && source.parsed.symbols.iter().filter(|s| s.name == sym.name).count() == 1
                    && !matches!(sym.kind, SymbolKind::Const | SymbolKind::Var | SymbolKind::Module)
            })
            .collect();
        let stride = (eligible.len() / PER_FILE).max(1);

        for &index in eligible.iter().step_by(stride).take(PER_FILE) {
            let sym = &source.parsed.symbols[index];
            let Some(&id) = ids.get(&(source.path.clone(), index)) else { continue };

            // Rename.
            let new_name = format!("{}Renamed", sym.name);
            let renamed = rename_in(&source.text, &sym.name, &new_name);
            if let Some(after) = step(&table, &[(source, &renamed)]) {
                let kept =
                    after.get(id).is_some_and(|entry| entry.name == new_name && entry.file == source.path);
                report.renames.record(kept);
                if !kept && std::env::var_os("KITO_CORPUS_VERBOSE").is_some() {
                    eprintln!(
                        "missed rename: {:?} {} in {} (body tokens {}, tokens {}) -> {:?}",
                        sym.kind,
                        sym.name,
                        source.path.display(),
                        sym.body_tokens,
                        sym.tokens,
                        after.get(id).map(|entry| (&entry.name, &entry.file))
                    );
                }
                if sym.body_tokens < kito_diff::ast::MIN_BODY_TOKENS {
                    report.small_renames.record(kept);
                }
            }

            // Rename and a small edit of the body in the same save. Not a
            // pure rename: recognized by resemblance, when there is enough text.
            if let Some(edited) = edit_in(&source.text, sym.range.clone(), &sym.name) {
                let edited = rename_in(&edited, &sym.name, &new_name);
                if let Some(after) = step(&table, &[(source, &edited)]) {
                    let kept = after.get(id).is_some_and(|entry| entry.name == new_name);
                    if sym.tokens >= kito_diff::ast::MIN_SHAPE_TOKENS {
                        report.edited_renames.record(kept);
                    } else {
                        report.small_edited_renames.record(kept);
                    }
                }
            }

            // Move to another file: a top-level symbol, or one that sits
            // directly in a namespace (which it leaves).
            let in_namespace =
                sym.parent.is_some_and(|p| source.parsed.symbols[p].kind == SymbolKind::Module);
            if (sym.parent.is_none() || in_namespace) && sources.len() > 1 && other.path != source.path {
                let cut = &source.text[sym.range.clone()];
                let without = format!("{}{}", &source.text[..sym.range.start], &source.text[sym.range.end..]);
                let with = format!("{}\n{cut}\n", other.text);
                if let Some(after) = step(&table, &[(source, &without), (other, &with)]) {
                    let kept =
                        after.get(id).is_some_and(|entry| entry.file == other.path && entry.parent.is_none());
                    report.moves.record(kept);
                    if in_namespace {
                        report.nested_moves.record(kept);
                    }
                    if !kept && std::env::var_os("KITO_CORPUS_VERBOSE").is_some() {
                        eprintln!(
                            "missed move: {:?} {} in {} (body tokens {}, tokens {})",
                            sym.kind,
                            sym.name,
                            source.path.display(),
                            sym.body_tokens,
                            sym.tokens
                        );
                    }
                }
            }

            // Replace with a different symbol from the other file.
            let stranger = other.parsed.symbols.iter().find(|s| {
                s.kind == sym.kind
                    && s.parent.is_none() == sym.parent.is_none()
                    && s.name != sym.name
                    && s.body != sym.body
                    && is_identifier(&s.name)
                    && !source.parsed.symbols.iter().any(|mine| mine.name == s.name)
            });
            if let Some(stranger) = stranger {
                let text = format!(
                    "{}{}{}",
                    &source.text[..sym.range.start],
                    &other.text[stranger.range.clone()],
                    &source.text[sym.range.end..]
                );
                if let Some(after) = step(&table, &[(source, &text)]) {
                    // The stranger must not inherit the ID of what it replaced.
                    let inherited = after.get(id).is_some_and(|entry| entry.name == stranger.name);
                    report.replacements.record(!inherited);
                    if inherited && std::env::var_os("KITO_CORPUS_VERBOSE").is_some() {
                        eprintln!(
                            "mistaken: {:?} {} for {} in {} (tokens {} vs {})",
                            sym.kind,
                            stranger.name,
                            sym.name,
                            source.path.display(),
                            stranger.tokens,
                            sym.tokens
                        );
                    }
                }
            }
        }
    }
    report
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "target/corpus".to_owned()));
    let languages: [(&str, &[Dialect]); 10] = [
        ("python", &[Dialect::Python]),
        ("javascript", &[Dialect::JavaScript]),
        ("typescript", &[Dialect::TypeScript, Dialect::Tsx]),
        ("java", &[Dialect::Java]),
        ("csharp", &[Dialect::CSharp]),
        ("c", &[Dialect::C]),
        ("cpp", &[Dialect::Cpp, Dialect::C]),
        ("go", &[Dialect::Go]),
        ("php", &[Dialect::Php]),
        ("rust", &[Dialect::Rust]),
    ];
    println!("| Language | Files | Symbols | Renames kept | …of small symbols | Renamed and edited | …of small symbols | Moves kept | …out of a namespace | Different symbols told apart |");
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut total = Report::default();
    for (name, dialects) in languages {
        let dir = root.join(name);
        if !dir.is_dir() {
            println!("| {name} | missing: run scripts/fetch-corpus.sh | | | | | | | | |");
            continue;
        }
        let mut paths = Vec::new();
        collect(&dir, dialects, &mut paths);
        let sources: Vec<Source> = paths
            .into_iter()
            .filter_map(|path| {
                let text = std::fs::read_to_string(&path).ok()?;
                let parsed = parse(dialect_for_path(&path)?, text.as_bytes())?;
                let rel = path.strip_prefix(&root).unwrap_or(&path).to_owned();
                (!parsed.has_errors && !parsed.symbols.is_empty()).then_some(Source {
                    path: rel,
                    text,
                    parsed,
                })
            })
            .take(MAX_FILES)
            .collect();
        let r = measure(&sources);
        println!(
            "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            r.files,
            r.symbols,
            r.renames.rate(),
            r.small_renames.rate(),
            r.edited_renames.rate(),
            r.small_edited_renames.rate(),
            r.moves.rate(),
            r.nested_moves.rate(),
            r.replacements.rate()
        );
        total.files += r.files;
        total.symbols += r.symbols;
        for (sum, part) in [
            (&mut total.renames, &r.renames),
            (&mut total.small_renames, &r.small_renames),
            (&mut total.edited_renames, &r.edited_renames),
            (&mut total.small_edited_renames, &r.small_edited_renames),
            (&mut total.moves, &r.moves),
            (&mut total.nested_moves, &r.nested_moves),
            (&mut total.replacements, &r.replacements),
        ] {
            sum.tried += part.tried;
            sum.passed += part.passed;
        }
    }
    println!(
        "| **all** | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
        total.files,
        total.symbols,
        total.renames.rate(),
        total.small_renames.rate(),
        total.edited_renames.rate(),
        total.small_edited_renames.rate(),
        total.moves.rate(),
        total.nested_moves.rate(),
        total.replacements.rate()
    );
}
