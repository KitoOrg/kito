//! From source text to the list of its symbols.

use crate::lang::Dialect;
use kito_core::{Fingerprint, Hash, Lang, SymbolKind};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

/// Number of hashes in a symbol's MinHash sketch.
pub const MINHASH_SIZE: usize = 32;
/// Tokens per shingle of the sketch.
const SHINGLE: usize = 3;
/// Separator between tokens when hashing, so that `ab c` and `a bc` differ.
const TOKEN_SEPARATOR: u8 = 0x1f;

/// The symbols of one source file.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedFile {
    pub lang: Lang,
    /// The parser had to recover from syntax errors: the symbols may be
    /// incomplete or wrong.
    pub has_errors: bool,
    /// In document order; a parent always comes before its children.
    pub symbols: Vec<ParsedSymbol>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedSymbol {
    pub kind: SymbolKind,
    pub name: String,
    /// Index of the enclosing symbol in [`ParsedFile::symbols`].
    pub parent: Option<usize>,
    /// Bytes of the whole definition.
    pub range: Range<usize>,
    /// Bytes of the definition up to its body: the part that declares it.
    pub header: Range<usize>,
    /// Line of the definition, from 1.
    pub line: u32,
    /// Hash of the signature tokens.
    pub signature: Hash,
    /// Hash of the body tokens, nested symbols excluded.
    pub body: Hash,
    /// Number of tokens in the body: tiny bodies are too common to identify
    /// a symbol by.
    pub body_tokens: usize,
    /// Number of tokens of the definition, its own name and nested symbols
    /// excluded: the ones the fingerprint is computed on.
    pub tokens: usize,
    /// MinHash sketch of those tokens: a renamed symbol keeps its fingerprint.
    pub fingerprint: Fingerprint,
}

impl ParsedFile {
    /// Name of a symbol qualified by its enclosing symbols: `Server.start`.
    pub fn qualified_name(&self, index: usize) -> String {
        let mut parts = vec![self.symbols[index].name.as_str()];
        let mut parent = self.symbols[index].parent;
        while let Some(p) = parent {
            parts.push(self.symbols[p].name.as_str());
            parent = self.symbols[p].parent;
        }
        parts.reverse();
        parts.join(".")
    }
}

/// What a capture of the symbols query means.
#[derive(Clone, Copy)]
enum Capture {
    Def(SymbolKind),
    Name,
    Scope,
    Signature,
    Body,
    Other,
}

struct Compiled {
    parser: Parser,
    query: Query,
    captures: Vec<Capture>,
    /// Per pattern: the text that joins `@scope` and `@name`.
    joiners: Vec<String>,
}

fn kind_from_capture(name: &str) -> Option<SymbolKind> {
    Some(match name {
        "module" => SymbolKind::Module,
        "class" => SymbolKind::Class,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "interface" => SymbolKind::Interface,
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "field" => SymbolKind::Field,
        "const" => SymbolKind::Const,
        "var" => SymbolKind::Var,
        "type_alias" => SymbolKind::TypeAlias,
        "block" => SymbolKind::Block,
        _ => return None,
    })
}

impl Compiled {
    fn new(dialect: Dialect) -> Compiled {
        let language = dialect.language();
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .unwrap_or_else(|e| panic!("grammar for {dialect:?} is not loadable: {e}"));
        let query = Query::new(&language, dialect.symbols_query())
            .unwrap_or_else(|e| panic!("symbols query for {dialect:?} does not compile: {e}"));
        let captures = query
            .capture_names()
            .iter()
            .map(|name| match *name {
                "name" => Capture::Name,
                "scope" => Capture::Scope,
                "sig" => Capture::Signature,
                "body" => Capture::Body,
                other => other
                    .strip_prefix("def.")
                    .and_then(kind_from_capture)
                    .map_or(Capture::Other, Capture::Def),
            })
            .collect();
        let joiners = (0..query.pattern_count())
            .map(|pattern| {
                query
                    .property_settings(pattern)
                    .iter()
                    .find(|p| &*p.key == "scope.joiner")
                    .and_then(|p| p.value.as_deref())
                    .unwrap_or(".")
                    .to_owned()
            })
            .collect();
        Compiled { parser, query, captures, joiners }
    }
}

thread_local! {
    /// Parsers and compiled queries, built once per thread and grammar.
    static COMPILED: RefCell<HashMap<Dialect, Compiled>> = RefCell::new(HashMap::new());
}

/// A symbol as the query found it, before nesting and hashing.
struct Found {
    pattern: usize,
    kind: SymbolKind,
    def: Range<usize>,
    line: u32,
    name: Range<usize>,
    scope: Option<Range<usize>>,
    signature: Vec<Range<usize>>,
    body: Option<Range<usize>>,
}

/// Parses `source` and extracts its symbols. `None` only if the parser gives
/// up entirely (it normally recovers from syntax errors and says so in
/// [`ParsedFile::has_errors`]).
pub fn parse(dialect: Dialect, source: &[u8]) -> Option<ParsedFile> {
    COMPILED.with(|cache| {
        let mut cache = cache.borrow_mut();
        let compiled = cache.entry(dialect).or_insert_with(|| Compiled::new(dialect));
        let tree = compiled.parser.parse(source, None)?;
        let root = tree.root_node();

        let mut found = find_symbols(compiled, root, source);
        // Outer definitions first; at equal ranges the query's pattern order decides.
        found.sort_by(|a, b| {
            (a.def.start, std::cmp::Reverse(a.def.end), a.pattern, a.name.start).cmp(&(
                b.def.start,
                std::cmp::Reverse(b.def.end),
                b.pattern,
                b.name.start,
            ))
        });
        let parents = nest(&found);
        let tokens = collect_tokens(root);

        let mut children: Vec<Vec<Range<usize>>> = vec![Vec::new(); found.len()];
        for (i, parent) in parents.iter().enumerate() {
            if let Some(p) = parent {
                children[*p].push(found[i].def.clone());
            }
        }

        let symbols = found
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let text = |r: &Range<usize>| String::from_utf8_lossy(&source[r.clone()]).trim().to_owned();
                let name = match &f.scope {
                    Some(scope) => format!("{}{}{}", text(scope), compiled.joiners[f.pattern], text(&f.name)),
                    None => text(&f.name),
                };
                // A function nested in a type is a method, in every language.
                let in_type = parents[i].is_some_and(|p| {
                    matches!(
                        found[p].kind,
                        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Enum
                    )
                });
                let kind =
                    if f.kind == SymbolKind::Function && in_type { SymbolKind::Method } else { f.kind };

                let own = |t: &Range<usize>| !children[i].iter().any(|c| within(t, c));
                let in_def = tokens_in(&tokens, &f.def);
                let body: Vec<&Range<usize>> = match &f.body {
                    Some(body) => tokens_in(&tokens, body).iter().filter(|t| own(t)).collect(),
                    // No body capture: everything but the name and the signature.
                    None => in_def
                        .iter()
                        .filter(|t| {
                            own(t) && !within(t, &f.name) && !f.signature.iter().any(|s| within(t, s))
                        })
                        .collect(),
                };
                let signature: Vec<&Range<usize>> =
                    f.signature.iter().flat_map(|s| tokens_in(&tokens, s).iter()).collect();
                let shape: Vec<&Range<usize>> =
                    in_def.iter().filter(|t| own(t) && !within(t, &f.name)).collect();

                ParsedSymbol {
                    kind,
                    name,
                    parent: parents[i],
                    range: f.def.clone(),
                    header: f.def.start..f.body.as_ref().map_or(f.def.end, |b| b.start.max(f.def.start)),
                    line: f.line,
                    signature: hash_tokens(source, &signature),
                    body: hash_tokens(source, &body),
                    body_tokens: body.len(),
                    tokens: shape.len(),
                    fingerprint: minhash(source, &shape),
                }
            })
            .collect();
        Some(ParsedFile { lang: dialect.lang(), has_errors: root.has_error(), symbols })
    })
}

/// Runs the symbols query. When several patterns capture the same name node
/// the widest definition wins (a decorated function over the bare one), then
/// the earliest pattern.
fn find_symbols(compiled: &Compiled, root: Node<'_>, source: &[u8]) -> Vec<Found> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&compiled.query, root, source);
    let mut by_name: HashMap<(usize, usize), Found> = HashMap::new();
    while let Some(m) = matches.next() {
        let (mut def, mut name, mut scope, mut body) = (None, None, None, None);
        let mut signature = Vec::new();
        for capture in m.captures() {
            let range = capture.node.byte_range();
            match compiled.captures[capture.index as usize] {
                Capture::Def(kind) => def = Some((kind, range, capture.node.start_position().row as u32 + 1)),
                Capture::Name => name = Some(range),
                Capture::Scope => scope = Some(range),
                Capture::Signature => signature.push(range),
                Capture::Body => body = Some(range),
                Capture::Other => {}
            }
        }
        let (Some((kind, def, line)), Some(name)) = (def, name) else { continue };
        let candidate =
            Found { pattern: m.pattern_index, kind, def, line, name: name.clone(), scope, signature, body };
        let better = |old: &Found| {
            let (new_len, old_len) = (candidate.def.len(), old.def.len());
            new_len > old_len || (new_len == old_len && candidate.pattern < old.pattern)
        };
        match by_name.get(&(name.start, name.end)) {
            Some(old) if !better(old) => {}
            _ => {
                by_name.insert((name.start, name.end), candidate);
            }
        }
    }
    by_name.into_values().collect()
}

/// Parent of each symbol: the nearest earlier symbol that strictly contains it.
fn nest(found: &[Found]) -> Vec<Option<usize>> {
    let mut parents = Vec::with_capacity(found.len());
    let mut stack: Vec<usize> = Vec::new();
    for (i, f) in found.iter().enumerate() {
        while let Some(&top) = stack.last() {
            let outer = &found[top].def;
            let contains = outer.start <= f.def.start && f.def.end <= outer.end && outer.len() > f.def.len();
            if contains {
                break;
            }
            stack.pop();
        }
        parents.push(stack.last().copied());
        stack.push(i);
    }
    parents
}

fn within(inner: &Range<usize>, outer: &Range<usize>) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// The leaves of the syntax tree, comments excluded, in document order.
/// Whitespace is not part of any leaf, so two formattings of the same code
/// have the same tokens.
fn collect_tokens(root: Node<'_>) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        let comment = node.kind().contains("comment");
        if !comment && node.child_count() == 0 && node.end_byte() > node.start_byte() {
            tokens.push(node.byte_range());
        }
        if !comment && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return tokens;
            }
        }
    }
}

/// The tokens that lie inside `range`.
fn tokens_in<'a>(tokens: &'a [Range<usize>], range: &Range<usize>) -> &'a [Range<usize>] {
    let start = tokens.partition_point(|t| t.start < range.start);
    let end = tokens.partition_point(|t| t.start < range.end);
    &tokens[start..end.max(start)]
}

fn hash_tokens(source: &[u8], tokens: &[&Range<usize>]) -> Hash {
    let mut buffer = Vec::with_capacity(tokens.iter().map(|t| t.len() + 1).sum());
    for token in tokens {
        buffer.extend_from_slice(&source[(*token).clone()]);
        buffer.push(TOKEN_SEPARATOR);
    }
    Hash::of(&buffer)
}

/// FNV-1a over the tokens of one shingle.
fn shingle_hash(source: &[u8], tokens: &[&Range<usize>]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |byte: u8| {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for token in tokens {
        source[(*token).clone()].iter().copied().for_each(&mut feed);
        feed(TOKEN_SEPARATOR);
    }
    h
}

/// The finalizer of splitmix64: turns one hash into many independent ones.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// MinHash sketch over the shingles of a token sequence: the share of equal
/// positions in two sketches estimates the Jaccard similarity of the two
/// shingle sets. No tokens, no sketch.
fn minhash(source: &[u8], tokens: &[&Range<usize>]) -> Fingerprint {
    if tokens.is_empty() {
        return Fingerprint::default();
    }
    let mut sketch = [u64::MAX; MINHASH_SIZE];
    let width = SHINGLE.min(tokens.len());
    for shingle in tokens.windows(width) {
        let h = shingle_hash(source, shingle);
        for (i, slot) in sketch.iter_mut().enumerate() {
            let v = mix(h ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
            *slot = (*slot).min(v);
        }
    }
    Fingerprint { sketch: sketch.iter().map(|v| (*v >> 32) as u32).collect() }
}
