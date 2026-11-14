//! `kito-semantic`: semantic level 2, the references between symbols.
//!
//! Level 1 knows what the symbols of a file are. Level 2 knows who uses
//! them: the call to `getUser` in another file is a use of the function
//! defined in `auth.ts`. That is what lets a rename made in one lane reach a
//! use added meanwhile in another.
//!
//! The answers come from a [`SemanticProvider`]. Two are here:
//!
//! - [`TreeSitterHeuristic`], always available: it reads the syntax only,
//!   finds the uses in the same file and through explicit imports, and is
//!   never sure (confidence 0.5 to 0.8);
//! - [`LspProvider`], which asks a language server, and is as sure as the
//!   server is;
//! - [`ScipImporter`], which reads an index made by an indexer over the
//!   whole repository, and answers for the files that have not changed
//!   since.
//!
//! Every answer carries its confidence and where it comes from: what to do
//! with a doubtful one is decided by who asks.

mod heuristic;
mod lsp;
mod scip;
mod text;

pub use heuristic::TreeSitterHeuristic;
pub use lsp::{LspConfig, LspProvider};
pub use scip::{ScipImporter, ScipIndex};
pub use text::{offset_at, position_at};

use kito_core::{Lang, Location, RefSource, Reference, SymbolId, SymbolTable, Tree};
use kito_parse::{dialect_for_path, parse_reusing, Parsed};
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Uses a provider is this sure of, or more, can be acted on without asking.
pub const SURE: f32 = 0.9;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("symbol {0} is not in this snapshot")]
    UnknownSymbol(SymbolId),
    #[error("language server `{server}`: {message}")]
    Server { server: String, message: String },
    #[error("language server `{server}` did not answer in {seconds} s")]
    Timeout { server: String, seconds: u64 },
    #[error("SCIP index: {0}")]
    Index(String),
    /// The server asked for the request to be made again. Never returned:
    /// the provider makes it again.
    #[error("{0}")]
    Again(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The contents of the files of a snapshot.
pub trait Files: Sync {
    fn read(&self, path: &Path) -> Option<Vec<u8>>;
}

impl<F: Fn(&Path) -> Option<Vec<u8>> + Sync> Files for F {
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        self(path)
    }
}

/// A state of the repository to ask about: a lane at some point.
#[derive(Clone, Copy)]
pub struct Snapshot<'a> {
    /// The main working directory: where a language server works.
    pub root: &'a Path,
    pub tree: &'a Tree,
    pub symbols: &'a SymbolTable,
    pub files: &'a dyn Files,
    /// The files that are not, under `root`, as this snapshot has them:
    /// their content here, or `None` for a file the snapshot does not have.
    /// A language server is told about these instead of being given a copy
    /// of the repository.
    pub overlay: &'a BTreeMap<PathBuf, Option<Vec<u8>>>,
}

/// One place where a symbol is used.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub file: PathBuf,
    /// Bytes of the name, in the file as the snapshot has it.
    pub range: Range<usize>,
    /// 0.0–1.0
    pub confidence: f32,
    pub source: RefSource,
}

/// Where a symbol is defined in a snapshot.
pub struct Definition {
    pub file: PathBuf,
    pub parsed: Parsed,
    /// Index of the symbol in the parsed file.
    pub index: usize,
    /// Which symbol of the history each parsed symbol is.
    pub ids: Vec<Option<SymbolId>>,
}

impl Definition {
    /// Bytes of the symbol's name in its definition.
    pub fn name_range(&self) -> Range<usize> {
        self.parsed.file.symbols[self.index].name_range.clone()
    }

    /// The name as written there: for a symbol the history calls
    /// `Server.Start`, the word `Start`.
    pub fn name(&self) -> &[u8] {
        &self.parsed.source()[self.name_range()]
    }
}

impl Snapshot<'_> {
    /// Reads and parses a file of the snapshot. `None` if it is not there or
    /// is not in a language Kito parses.
    pub fn parse(&self, path: &Path) -> Option<Parsed> {
        let dialect = dialect_for_path(path)?;
        self.tree.get(path)?;
        parse_reusing(dialect, &self.files.read(path)?, None)
    }

    /// Finds the definition of a symbol.
    pub fn definition(&self, sym: SymbolId) -> Result<Definition> {
        let entry = self.symbols.get(sym).ok_or(Error::UnknownSymbol(sym))?;
        let parsed = self.parse(&entry.file).ok_or(Error::UnknownSymbol(sym))?;
        let ids = kito_diff::ast::identify(self.symbols, &entry.file, &parsed.file);
        let index = ids.iter().position(|id| *id == Some(sym)).ok_or(Error::UnknownSymbol(sym))?;
        Ok(Definition { file: entry.file.clone(), parsed, index, ids })
    }
}

/// A source of references between symbols.
#[allow(async_fn_in_trait)]
pub trait SemanticProvider {
    /// The languages it answers for.
    fn languages(&self) -> &[Lang];
    /// The references that leave the symbols of a file.
    async fn references(&self, snap: &Snapshot<'_>, file: &Path) -> Result<Vec<Reference>>;
    /// Who uses this symbol: every place its name is written to mean it,
    /// its own definition excluded.
    async fn usages(&self, snap: &Snapshot<'_>, sym: SymbolId) -> Result<Vec<Usage>>;
}

/// The reference a usage is, for the data model: from the symbol the use is
/// written in. `None` for a use outside every symbol (an import, a
/// statement at the top of a file).
pub fn reference(snap: &Snapshot<'_>, to: SymbolId, usage: &Usage) -> Option<Reference> {
    let parsed = snap.parse(&usage.file)?;
    let ids = kito_diff::ast::identify(snap.symbols, &usage.file, &parsed.file);
    // The innermost symbol that contains the use: parents come first.
    let from = parsed
        .file
        .symbols
        .iter()
        .zip(&ids)
        .filter(|(sym, _)| sym.range.start <= usage.range.start && usage.range.end <= sym.range.end)
        .filter_map(|(_, id)| *id)
        .next_back()?;
    let line = 1 + parsed.source()[..usage.range.start].iter().filter(|&&b| b == b'\n').count() as u32;
    Some(Reference {
        from,
        to,
        site: Location {
            file: usage.file.clone(),
            line,
            start: usage.range.start as u32,
            end: usage.range.end as u32,
        },
        confidence: usage.confidence,
        source: usage.source.clone(),
    })
}

/// An index imported into a repository: the file it is in, and the content
/// each file of the repository had when it was made.
pub struct ScipSource {
    pub index: PathBuf,
    pub files: std::collections::HashMap<PathBuf, kito_core::BlobId>,
}

/// The providers of a repository, asked in order of how much they know: the
/// language server of the symbol's language if there is one and it answers;
/// an imported index for the files that have not changed since it was made;
/// the heuristic for the rest.
#[derive(Default)]
pub struct Semantic {
    pub heuristic: TreeSitterHeuristic,
    pub servers: Vec<LspProvider>,
    scip: Option<ScipSource>,
    /// The index, read the first time it is needed.
    index: std::sync::OnceLock<Option<ScipImporter>>,
}

impl Semantic {
    pub fn new(servers: Vec<LspProvider>) -> Semantic {
        Semantic { servers, ..Semantic::default() }
    }

    /// The same providers, with an imported index.
    pub fn with_scip(mut self, source: ScipSource) -> Semantic {
        self.scip = Some(source);
        self.index = Default::default();
        self
    }

    /// The imported index, if there is one and it can be read.
    pub fn index(&self) -> Option<&ScipImporter> {
        self.index
            .get_or_init(|| {
                let source = self.scip.as_ref()?;
                let index = ScipIndex::parse(&std::fs::read(&source.index).ok()?).ok()?;
                Some(ScipImporter::new(index, source.files.clone()))
            })
            .as_ref()
    }

    fn server_for(&self, lang: Lang) -> Option<&LspProvider> {
        self.servers.iter().find(|server| server.available() && server.languages().contains(&lang))
    }

    /// `true` if a language server is configured for this language, and is
    /// not known to be missing.
    pub fn has_server(&self, lang: Lang) -> bool {
        self.server_for(lang).is_some()
    }

    /// Who uses `sym`. A language server that fails is not an error here:
    /// the next provider answers in its place, and `problems` says so.
    pub async fn usages(
        &self,
        snap: &Snapshot<'_>,
        sym: SymbolId,
        problems: &mut Vec<String>,
    ) -> Result<Vec<Usage>> {
        let entry = snap.symbols.get(sym).ok_or(Error::UnknownSymbol(sym))?;
        if let Some(server) = self.server_for(entry.lang) {
            match server.usages(snap, sym).await {
                Ok(found) => return Ok(found),
                Err(e) => problems.push(e.to_string()),
            }
        }
        let read = self.heuristic.usages_of(snap, sym)?;
        let Some(index) = self.index() else { return Ok(read) };
        let Some(mut found) = index.usages_of(snap, sym)? else { return Ok(read) };
        // What the index does not know as it is now is read from the syntax.
        found.extend(read.into_iter().filter(|usage| !index.knows(snap, &usage.file)));
        found.sort_by(|a, b| (&a.file, a.range.start).cmp(&(&b.file, b.range.start)));
        Ok(found)
    }

    /// Stops the language servers.
    pub async fn shutdown(&self) {
        for server in &self.servers {
            server.shutdown().await;
        }
    }
}
