//! The provider that reads a SCIP index.
//!
//! SCIP is a format for the output of an indexer: for every file, where
//! each symbol is defined and where it is used. An indexer runs over a whole
//! repository in one go, which is how a large one gets its references
//! without a language server kept running. The index is a picture of the
//! files at the moment it was made: an answer is taken from it only for the
//! files that are still as they were then, and the others are left to who
//! can read them now.
//!
//! Only what is needed is read from the format: documents, and in them the
//! occurrences with their range, symbol and roles.

use crate::text::offset_at;
use crate::{Error, Result, Snapshot, Usage};
use kito_core::{BlobId, RefSource, SymbolId};
use prost::Message;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The messages of `scip.proto`, as far as they are read here.
mod wire {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Index {
        #[prost(message, repeated, tag = "2")]
        pub documents: Vec<Document>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Document {
        #[prost(string, tag = "1")]
        pub relative_path: String,
        #[prost(message, repeated, tag = "2")]
        pub occurrences: Vec<Occurrence>,
        /// 1: UTF-8, 2: UTF-16, 3: UTF-32 code units from the line start.
        #[prost(int32, tag = "6")]
        pub position_encoding: i32,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Occurrence {
        /// `[line, start, end]` or `[start line, start, end line, end]`.
        #[prost(int32, repeated, tag = "1")]
        pub range: Vec<i32>,
        #[prost(string, tag = "2")]
        pub symbol: String,
        #[prost(int32, tag = "3")]
        pub symbol_roles: i32,
        #[prost(message, optional, tag = "8")]
        pub single_line_range: Option<SingleLineRange>,
        #[prost(message, optional, tag = "9")]
        pub multi_line_range: Option<MultiLineRange>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SingleLineRange {
        #[prost(int32, tag = "1")]
        pub line: i32,
        #[prost(int32, tag = "2")]
        pub start_character: i32,
        #[prost(int32, tag = "3")]
        pub end_character: i32,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct MultiLineRange {
        #[prost(int32, tag = "1")]
        pub start_line: i32,
        #[prost(int32, tag = "2")]
        pub start_character: i32,
        #[prost(int32, tag = "3")]
        pub end_line: i32,
        #[prost(int32, tag = "4")]
        pub end_character: i32,
    }
}

/// The role bit of an occurrence that defines its symbol.
const DEFINITION: i32 = 1;

/// A place in a document: line and character, start and end.
type Span = ((u32, u32), (u32, u32));

struct Document {
    path: PathBuf,
    /// As the index says: 1 UTF-8, 2 UTF-16, 3 UTF-32; 0 if it does not say.
    encoding: i32,
    /// The symbols the document defines, by where their name starts.
    definitions: HashMap<(u32, u32), String>,
}

/// A SCIP index in memory: who is defined where, and used where.
pub struct ScipIndex {
    documents: Vec<Document>,
    /// Per symbol, the occurrences that are not its definition.
    uses: HashMap<String, Vec<(u32, Span)>>,
}

impl wire::Occurrence {
    fn span(&self) -> Option<Span> {
        let at =
            |line: i32, character: i32| Some((u32::try_from(line).ok()?, u32::try_from(character).ok()?));
        if let Some(range) = &self.single_line_range {
            return Some((at(range.line, range.start_character)?, at(range.line, range.end_character)?));
        }
        if let Some(range) = &self.multi_line_range {
            return Some((
                at(range.start_line, range.start_character)?,
                at(range.end_line, range.end_character)?,
            ));
        }
        match self.range[..] {
            [line, start, end] => Some((at(line, start)?, at(line, end)?)),
            [line, start, end_line, end] => Some((at(line, start)?, at(end_line, end)?)),
            _ => None,
        }
    }
}

impl ScipIndex {
    /// Reads an index from the bytes of an `index.scip`.
    pub fn parse(bytes: &[u8]) -> Result<ScipIndex> {
        let index = wire::Index::decode(bytes).map_err(|e| Error::Index(format!("not a SCIP index: {e}")))?;
        let mut documents = Vec::with_capacity(index.documents.len());
        let mut uses: HashMap<String, Vec<(u32, Span)>> = HashMap::new();
        for (number, document) in index.documents.into_iter().enumerate() {
            let mut definitions = HashMap::new();
            for occurrence in &document.occurrences {
                let Some(span) = occurrence.span() else { continue };
                if occurrence.symbol.is_empty() {
                    continue;
                }
                // A local symbol has a name only inside its document.
                let symbol = match occurrence.symbol.starts_with("local ") {
                    true => format!("{number}:{}", occurrence.symbol),
                    false => occurrence.symbol.clone(),
                };
                match occurrence.symbol_roles & DEFINITION != 0 {
                    true => {
                        definitions.insert(span.0, symbol);
                    }
                    false => uses.entry(symbol).or_default().push((number as u32, span)),
                }
            }
            documents.push(Document {
                path: PathBuf::from(document.relative_path),
                encoding: document.position_encoding,
                definitions,
            });
        }
        Ok(ScipIndex { documents, uses })
    }

    /// The files the index is about.
    pub fn files(&self) -> impl Iterator<Item = &Path> {
        self.documents.iter().map(|document| document.path.as_path())
    }

    /// How many symbols have at least one use.
    pub fn symbols(&self) -> usize {
        self.uses.len()
    }
}

/// The byte offset of a line and a character counted in the units the
/// document's encoding says.
fn offset(content: &[u8], at: (u32, u32), encoding: i32) -> Option<usize> {
    if encoding == 2 {
        return offset_at(content, at.0, at.1);
    }
    let mut start = 0;
    for _ in 0..at.0 {
        start += content[start..].iter().position(|&b| b == b'\n')? + 1;
    }
    let end = content[start..].iter().position(|&b| b == b'\n').map_or(content.len(), |n| start + n);
    let line = &content[start..end];
    match encoding {
        // Code points.
        3 => {
            let text = String::from_utf8_lossy(line);
            let bytes: usize = text.chars().take(at.1 as usize).map(char::len_utf8).sum();
            Some(start + bytes.min(line.len()))
        }
        // Bytes.
        _ => Some(start + (at.1 as usize).min(line.len())),
    }
}

/// The ways characters may be counted in a document: the one it declares,
/// or each of them in turn if it declares none.
fn encodings(declared: i32) -> &'static [i32] {
    match declared {
        1 => &[1],
        2 => &[2],
        3 => &[3],
        _ => &[2, 1, 3],
    }
}

/// An index, and the content each of its files had when it was made.
pub struct ScipImporter {
    index: ScipIndex,
    by_path: HashMap<PathBuf, u32>,
    files: HashMap<PathBuf, BlobId>,
}

impl ScipImporter {
    /// `files`: for the files of the index, the content they had when the
    /// index was made. A file that is not there is taken as changed.
    pub fn new(index: ScipIndex, files: HashMap<PathBuf, BlobId>) -> ScipImporter {
        let by_path = index.documents.iter().enumerate().map(|(n, d)| (d.path.clone(), n as u32)).collect();
        ScipImporter { index, by_path, files }
    }

    pub fn index(&self) -> &ScipIndex {
        &self.index
    }

    /// `true` if the index speaks of this file as the snapshot has it.
    pub fn knows(&self, snap: &Snapshot<'_>, path: &Path) -> bool {
        self.by_path.contains_key(path)
            && snap.tree.get(path).is_some_and(|entry| self.files.get(path) == Some(&entry.blob))
    }

    /// Who uses `sym`, in the files that are as they were when the index
    /// was made. `None`: the index cannot say, because the file that
    /// defines the symbol changed since, or it does not know the symbol.
    pub fn usages_of(&self, snap: &Snapshot<'_>, sym: SymbolId) -> Result<Option<Vec<Usage>>> {
        let def = snap.definition(sym)?;
        if !self.knows(snap, &def.file) {
            return Ok(None);
        }
        let document = &self.index.documents[self.by_path[&def.file] as usize];
        let (source, at) = (def.parsed.source(), def.name_range().start);
        let counted = encodings(document.encoding);
        let symbol = document
            .definitions
            .iter()
            .find(|(start, _)| counted.iter().any(|encoding| offset(source, **start, *encoding) == Some(at)))
            .map(|(_, symbol)| symbol);
        let Some(symbol) = symbol else { return Ok(None) };

        let name = def.name();
        let mut contents: HashMap<u32, Option<Vec<u8>>> = HashMap::new();
        let mut usages = Vec::new();
        for (number, span) in self.index.uses.get(symbol).map(Vec::as_slice).unwrap_or_default() {
            let used_in = &self.index.documents[*number as usize];
            if !self.knows(snap, &used_in.path) {
                continue;
            }
            let Some(content) = contents.entry(*number).or_insert_with(|| snap.files.read(&used_in.path))
            else {
                continue;
            };
            let candidates = encodings(used_in.encoding);
            // Where the name is written: an index may also point at what
            // refers to the symbol under another name.
            let range = candidates.iter().find_map(|encoding| {
                let range = offset(content, span.0, *encoding)?..offset(content, span.1, *encoding)?;
                (content.get(range.clone()) == Some(name)).then_some(range)
            });
            if let Some(range) = range {
                usages.push(Usage {
                    file: used_in.path.clone(),
                    range,
                    confidence: 1.0,
                    source: RefSource::Scip,
                });
            }
        }
        usages.sort_by(|a, b| (&a.file, a.range.start).cmp(&(&b.file, b.range.start)));
        usages.dedup_by(|a, b| a.file == b.file && a.range == b.range);
        Ok(Some(usages))
    }
}
