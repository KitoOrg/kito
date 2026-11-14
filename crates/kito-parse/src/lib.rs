//! `kito-parse`: semantic level 1.
//!
//! Tree-sitter gives the structure of a source file; a per-language query
//! (`queries/<lang>/symbols.scm`) says which nodes are symbols and where their
//! name, signature and body are. From that this crate extracts the symbols of
//! a file together with hashes that ignore whitespace, comments and
//! formatting, so that reformatting a file changes nothing.
//!
//! Adding a language means adding its grammar and its query file to
//! [`lang`]; the extraction code is the same for every language.

mod extract;
pub mod lang;

pub use extract::{parse, parse_reusing, Parsed, ParsedFile, ParsedSymbol, MINHASH_SIZE};
pub use lang::{dialect_for_path, lang_for_path, Dialect};
