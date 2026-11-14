//! The provider that reads only the syntax.
//!
//! It knows two things: a name used in the file that defines it most likely
//! means that definition, and a file that imports a name from another file
//! uses what that file defines. It never compiles anything, so it is never
//! sure: its confidence goes from 0.5 to 0.8, below what is needed to act
//! without asking.
//!
//! Imports are followed for JavaScript and TypeScript (relative module
//! paths), Python (`from … import …`) and Go (files of the same directory,
//! and `pkg.Name` through the package's import); in the other languages it
//! sees the uses in the same file only.

use crate::{Result, SemanticProvider, Snapshot, Usage};
use kito_core::{Lang, Location, RefSource, Reference, SymbolId};
use kito_parse::{dialect_for_path, parse_reusing, Dialect, Parsed};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

/// The provider that is always there.
#[derive(Clone, Copy, Debug, Default)]
pub struct TreeSitterHeuristic;

const LANGUAGES: &[Lang] = &[
    Lang::Python,
    Lang::JavaScript,
    Lang::TypeScript,
    Lang::Java,
    Lang::CSharp,
    Lang::C,
    Lang::Cpp,
    Lang::Go,
    Lang::Php,
    Lang::Rust,
];

/// A plain name used in the file that defines it.
const SAME_FILE: f32 = 0.8;
/// The same, when the file has several symbols with that name.
const SAME_FILE_NAMESAKES: f32 = 0.6;
/// A member used inside the type that defines it, or elsewhere in its file.
const MEMBER_INSIDE: f32 = 0.7;
const MEMBER_ELSEWHERE: f32 = 0.5;
/// The name in an import that leads to the defining file, and its uses in
/// the importing file.
const IMPORTED: f32 = 0.8;
const IMPORTED_USE: f32 = 0.7;
/// An import whose module can only be matched by the end of its path.
const IMPORTED_BY_GUESS: f32 = 0.6;

const JS_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];

fn text<'a>(source: &'a [u8], range: &Range<usize>) -> &'a [u8] {
    &source[range.clone()]
}

/// Languages whose files import each other.
fn family(dialect: Dialect) -> Dialect {
    match dialect {
        Dialect::Tsx | Dialect::JavaScript => Dialect::TypeScript,
        other => other,
    }
}

/// `a/b/../c/./d` → `a/c/d`. `None` if it climbs out of the repository.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

fn with_extension_added(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(extension);
    PathBuf::from(name)
}

/// The files a relative module path of JavaScript or TypeScript may mean.
fn js_candidates(from: &Path, module: &str) -> Vec<PathBuf> {
    // Packages and path aliases are not ours to guess.
    if !module.starts_with('.') {
        return Vec::new();
    }
    let Some(base) = normalize(&from.parent().unwrap_or(Path::new("")).join(module)) else {
        return Vec::new();
    };
    let mut candidates = vec![base.clone()];
    for extension in JS_EXTENSIONS {
        candidates.push(with_extension_added(&base, extension));
        candidates.push(base.join(format!("index.{extension}")));
    }
    // `./users.js` written for `users.ts`.
    if base.extension().is_some_and(|e| JS_EXTENSIONS.iter().any(|known| e == *known)) {
        for extension in JS_EXTENSIONS {
            candidates.push(base.with_extension(extension));
        }
    }
    candidates
}

/// How surely a Python module name, written in `from`, means `target`.
fn python_module_is(from: &Path, module: &str, target: &Path) -> Option<f32> {
    let dots = module.chars().take_while(|c| *c == '.').count();
    let parts: Vec<&str> = module[dots..].split('.').filter(|part| !part.is_empty()).collect();
    let mut tail = PathBuf::new();
    parts.iter().for_each(|part| tail.push(part));
    if dots > 0 {
        let mut base = from.parent().unwrap_or(Path::new("")).to_path_buf();
        for _ in 1..dots {
            if !base.pop() {
                return None;
            }
        }
        let package = base.join(&tail);
        let hit = match parts.is_empty() {
            true => package.join("__init__.py") == target,
            false => with_extension_added(&package, "py") == target || package.join("__init__.py") == target,
        };
        return hit.then_some(IMPORTED);
    }
    if parts.is_empty() {
        return None;
    }
    // An absolute name: the root it is counted from is not known here.
    let hit =
        target.ends_with(with_extension_added(&tail, "py")) || target.ends_with(tail.join("__init__.py"));
    hit.then_some(IMPORTED_BY_GUESS)
}

/// How surely a Go import path means the package in `dir`.
fn go_import_is(module: &str, dir: &Path) -> Option<f32> {
    let module: Vec<&str> = module.split('/').filter(|part| !part.is_empty()).collect();
    let dir: Vec<String> = dir.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let common = module.iter().rev().zip(dir.iter().rev()).take_while(|(a, b)| **a == b.as_str()).count();
    match common {
        0 => None,
        // The whole directory is the end of the import path.
        n if n == dir.len() => Some(IMPORTED_USE),
        1 => None,
        _ => Some(IMPORTED_BY_GUESS),
    }
}

/// The uses found so far, one per place: the surest answer wins.
#[derive(Default)]
struct Found(BTreeMap<(PathBuf, usize), Usage>);

impl Found {
    fn add(&mut self, file: &Path, range: &Range<usize>, confidence: f32) {
        let usage = Usage {
            file: file.to_owned(),
            range: range.clone(),
            confidence,
            source: RefSource::TreeSitterHeuristic,
        };
        self.0
            .entry((file.to_owned(), range.start))
            .and_modify(|known| {
                if known.confidence < confidence {
                    known.confidence = confidence;
                }
            })
            .or_insert(usage);
    }
}

/// Uses, in the file `path`, of the top-level symbol `name` of `target`.
fn uses_elsewhere(target: &Path, name: &[u8], path: &Path, parsed: &Parsed, found: &mut Found) {
    let source = parsed.source();
    let identifiers = parsed.identifiers();
    // A file that defines the name itself means its own.
    let defines =
        parsed.file.symbols.iter().any(|sym| sym.parent.is_none() && text(source, &sym.name_range) == name);
    let plain = |confidence: f32, except: Option<&Range<usize>>, found: &mut Found| {
        for id in &identifiers {
            if !id.member && text(source, &id.range) == name && Some(&id.range) != except {
                found.add(path, &id.range, confidence);
            }
        }
    };
    let through = |namespace: &[u8], confidence: f32, found: &mut Found| {
        for id in &identifiers {
            let qualified = id.qualifier.as_ref().is_some_and(|q| text(source, q) == namespace);
            if id.member && qualified && text(source, &id.range) == name {
                found.add(path, &id.range, confidence);
            }
        }
    };
    let Some(dialect) = dialect_for_path(path) else { return };
    match family(dialect) {
        Dialect::TypeScript | Dialect::Python => {
            for import in parsed.imports() {
                let sure = match family(dialect) {
                    Dialect::Python => python_module_is(path, &import.module, target),
                    _ => js_candidates(path, &import.module).iter().any(|c| c == target).then_some(IMPORTED),
                };
                let Some(sure) = sure else { continue };
                for imported in &import.names {
                    if text(source, &imported.name) != name {
                        continue;
                    }
                    found.add(path, &imported.name, sure);
                    // Under another name the uses are not written `name`.
                    if imported.alias.is_none() && !defines {
                        plain(sure - 0.1, Some(&imported.name), found);
                    }
                }
                if let Some(namespace) = &import.namespace {
                    through(text(source, namespace), sure - 0.1, found);
                }
            }
        }
        Dialect::Go => {
            let (here, there) =
                (path.parent().unwrap_or(Path::new("")), target.parent().unwrap_or(Path::new("")));
            if here == there {
                if !defines {
                    plain(IMPORTED_USE, None, found);
                }
                return;
            }
            for import in parsed.imports() {
                let Some(sure) = go_import_is(&import.module, there) else { continue };
                let default = import.module.rsplit('/').next().unwrap_or_default().as_bytes().to_vec();
                let namespace = import.namespace.as_ref().map_or(default, |r| text(source, r).to_vec());
                through(&namespace, sure, found);
            }
        }
        _ => {}
    }
}

impl TreeSitterHeuristic {
    /// Who uses `sym`, as far as the syntax tells.
    pub fn usages_of(&self, snap: &Snapshot<'_>, sym: SymbolId) -> Result<Vec<Usage>> {
        let def = snap.definition(sym)?;
        let source = def.parsed.source();
        let symbols = &def.parsed.file.symbols;
        let me = &symbols[def.index];
        let name = def.name().to_vec();
        // The names of the definitions are not uses.
        let defined_at: HashSet<usize> = symbols.iter().map(|s| s.name_range.start).collect();
        let namesakes = symbols.iter().filter(|s| text(source, &s.name_range) == name.as_slice()).count();
        // A method, a field, or a symbol whose name the history writes with
        // its owner (`Server.Start`): reached through something else.
        let member = me.parent.is_some() || me.name.as_bytes() != name.as_slice();
        let owner = me.parent.map(|p| symbols[p].range.clone());

        let mut found = Found::default();
        for id in def.parsed.identifiers() {
            if text(source, &id.range) != name.as_slice() || defined_at.contains(&id.range.start) {
                continue;
            }
            let inside = owner.as_ref().is_some_and(|owner| owner.contains(&id.range.start));
            let confidence = match (member, id.member) {
                (false, false) if namesakes > 1 => SAME_FILE_NAMESAKES,
                (false, false) => SAME_FILE,
                (false, true) => continue,
                (true, _) if inside => MEMBER_INSIDE,
                (true, true) => MEMBER_ELSEWHERE,
                (true, false) => continue,
            };
            found.add(&def.file, &id.range, confidence);
        }

        // Through imports: only what a file exports by its plain name.
        if !member {
            let mine = dialect_for_path(&def.file).map(family);
            for (path, _) in snap.tree.iter() {
                let Some(dialect) = dialect_for_path(path) else { continue };
                if path == def.file || Some(family(dialect)) != mine {
                    continue;
                }
                let Some(content) = snap.files.read(path) else { continue };
                if !content.windows(name.len().max(1)).any(|window| window == name.as_slice()) {
                    continue;
                }
                if let Some(parsed) = parse_reusing(dialect, &content, None) {
                    uses_elsewhere(&def.file, &name, path, &parsed, &mut found);
                }
            }
        }
        Ok(found.0.into_values().collect())
    }

    /// The references that leave the symbols of `file`, as far as the
    /// syntax tells: to the symbols of the same file and to the ones it
    /// imports by name.
    pub fn references_in(&self, snap: &Snapshot<'_>, file: &Path) -> Vec<Reference> {
        let Some(parsed) = snap.parse(file) else { return Vec::new() };
        let Some(dialect) = dialect_for_path(file) else { return Vec::new() };
        let source = parsed.source();
        let symbols = &parsed.file.symbols;
        let ids = kito_diff::ast::identify(snap.symbols, file, &parsed.file);

        // What a plain name means in this file.
        let mut meaning: HashMap<Vec<u8>, Option<(SymbolId, f32)>> = HashMap::new();
        for (sym, id) in symbols.iter().zip(&ids) {
            if sym.parent.is_none() {
                // Two definitions of a name: neither is meant for sure.
                meaning
                    .entry(text(source, &sym.name_range).to_vec())
                    .and_modify(|known| *known = None)
                    .or_insert(id.map(|id| (id, SAME_FILE)));
            }
        }
        let exported = |target: &Path, name: &[u8]| -> Option<SymbolId> {
            let name = std::str::from_utf8(name).ok()?;
            match snap.symbols.find(target, name).as_slice() {
                [one] if snap.symbols.get(*one).is_some_and(|entry| entry.parent.is_none()) => Some(*one),
                _ => None,
            }
        };
        match family(dialect) {
            Dialect::TypeScript | Dialect::Python => {
                for import in parsed.imports() {
                    let target: Option<(PathBuf, f32)> = match family(dialect) {
                        Dialect::Python => snap.tree.iter().find_map(|(path, _)| {
                            Some((path.to_owned(), python_module_is(file, &import.module, path)?))
                        }),
                        _ => js_candidates(file, &import.module)
                            .into_iter()
                            .find(|candidate| snap.tree.contains(candidate))
                            .map(|path| (path, IMPORTED)),
                    };
                    let Some((target, sure)) = target else { continue };
                    for imported in &import.names {
                        let Some(to) = exported(&target, text(source, &imported.name)) else { continue };
                        let local = imported.alias.as_ref().unwrap_or(&imported.name);
                        meaning.entry(text(source, local).to_vec()).or_insert(Some((to, sure - 0.1)));
                    }
                }
            }
            Dialect::Go => {
                let dir = file.parent().unwrap_or(Path::new(""));
                for (path, _) in snap.tree.under(dir) {
                    let same_package = path.parent() == Some(dir) && path != file;
                    if !same_package || dialect_for_path(path) != Some(Dialect::Go) {
                        continue;
                    }
                    for (id, entry) in snap.symbols.in_file(path) {
                        if entry.parent.is_none() && !entry.name.contains('.') {
                            meaning
                                .entry(entry.name.clone().into_bytes())
                                .or_insert(Some((id, IMPORTED_USE)));
                        }
                    }
                }
            }
            _ => {}
        }

        let defined_at: HashSet<usize> = symbols.iter().map(|s| s.name_range.start).collect();
        let mut references = Vec::new();
        for id in parsed.identifiers() {
            if id.member || defined_at.contains(&id.range.start) {
                continue;
            }
            let Some(Some((to, confidence))) = meaning.get(text(source, &id.range)) else { continue };
            // The innermost symbol the use is written in.
            let from = symbols
                .iter()
                .zip(&ids)
                .filter(|(sym, _)| sym.range.start <= id.range.start && id.range.end <= sym.range.end)
                .filter_map(|(_, id)| *id)
                .next_back();
            let Some(from) = from else { continue };
            if from == *to {
                continue; // a symbol that mentions itself
            }
            let line = 1 + source[..id.range.start].iter().filter(|&&b| b == b'\n').count() as u32;
            references.push(Reference {
                from,
                to: *to,
                site: Location {
                    file: file.to_owned(),
                    line,
                    start: id.range.start as u32,
                    end: id.range.end as u32,
                },
                confidence: *confidence,
                source: RefSource::TreeSitterHeuristic,
            });
        }
        references
    }
}

impl SemanticProvider for TreeSitterHeuristic {
    fn languages(&self) -> &[Lang] {
        LANGUAGES
    }

    async fn references(&self, snap: &Snapshot<'_>, file: &Path) -> Result<Vec<Reference>> {
        Ok(self.references_in(snap, file))
    }

    async fn usages(&self, snap: &Snapshot<'_>, sym: SymbolId) -> Result<Vec<Usage>> {
        self.usages_of(snap, sym)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_paths_are_matched_to_files() {
        let from = Path::new("src/web/profile.ts");
        assert!(js_candidates(from, "../auth").contains(&PathBuf::from("src/auth.ts")));
        assert!(js_candidates(from, "./users").contains(&PathBuf::from("src/web/users/index.tsx")));
        assert!(js_candidates(from, "../auth.js").contains(&PathBuf::from("src/auth.ts")));
        assert!(js_candidates(from, "react").is_empty());
        assert!(js_candidates(Path::new("a.ts"), "../../x").is_empty());

        let from = Path::new("app/web/views.py");
        assert_eq!(python_module_is(from, ".models", Path::new("app/web/models.py")), Some(IMPORTED));
        assert_eq!(python_module_is(from, "..db.users", Path::new("app/db/users.py")), Some(IMPORTED));
        assert_eq!(python_module_is(from, "..db", Path::new("app/db/__init__.py")), Some(IMPORTED));
        assert_eq!(
            python_module_is(from, "app.db.users", Path::new("app/db/users.py")),
            Some(IMPORTED_BY_GUESS)
        );
        assert_eq!(python_module_is(from, "users", Path::new("app/db/users.py")), Some(IMPORTED_BY_GUESS));
        assert_eq!(python_module_is(from, ".models", Path::new("app/models.py")), None);
        assert_eq!(python_module_is(from, "os.path", Path::new("app/db/users.py")), None);

        assert_eq!(
            go_import_is("github.com/acme/app/internal/users", Path::new("internal/users")),
            Some(IMPORTED_USE)
        );
        assert_eq!(go_import_is("app/users", Path::new("services/app/users")), Some(IMPORTED_BY_GUESS));
        assert_eq!(go_import_is("app/users", Path::new("other/users")), None);
        assert_eq!(go_import_is("fmt", Path::new("users")), None);
    }
}
