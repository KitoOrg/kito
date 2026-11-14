//! The supported languages: grammar, file extensions and symbol query.

use kito_core::Lang;
use std::path::Path;

/// A concrete grammar. TypeScript has two (plain and TSX); every other
/// language has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dialect {
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Java,
    CSharp,
    C,
    Cpp,
    Go,
    Php,
    Rust,
}

impl Dialect {
    pub const ALL: [Dialect; 11] = [
        Dialect::Python,
        Dialect::JavaScript,
        Dialect::TypeScript,
        Dialect::Tsx,
        Dialect::Java,
        Dialect::CSharp,
        Dialect::C,
        Dialect::Cpp,
        Dialect::Go,
        Dialect::Php,
        Dialect::Rust,
    ];

    /// The language of the data model this grammar belongs to.
    pub fn lang(self) -> Lang {
        match self {
            Dialect::Python => Lang::Python,
            Dialect::JavaScript => Lang::JavaScript,
            Dialect::TypeScript | Dialect::Tsx => Lang::TypeScript,
            Dialect::Java => Lang::Java,
            Dialect::CSharp => Lang::CSharp,
            Dialect::C => Lang::C,
            Dialect::Cpp => Lang::Cpp,
            Dialect::Go => Lang::Go,
            Dialect::Php => Lang::Php,
            Dialect::Rust => Lang::Rust,
        }
    }

    /// File extensions (lowercase, without the dot) parsed with this grammar.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Dialect::Python => &["py", "pyi"],
            Dialect::JavaScript => &["js", "mjs", "cjs", "jsx"],
            Dialect::TypeScript => &["ts", "mts", "cts"],
            Dialect::Tsx => &["tsx"],
            Dialect::Java => &["java"],
            Dialect::CSharp => &["cs"],
            Dialect::C => &["c", "h"],
            Dialect::Cpp => &["cc", "cpp", "cxx", "c++", "hpp", "hh", "hxx", "h++"],
            Dialect::Go => &["go"],
            Dialect::Php => &["php"],
            Dialect::Rust => &["rs"],
        }
    }

    pub(crate) fn language(self) -> tree_sitter::Language {
        match self {
            Dialect::Python => tree_sitter_python::LANGUAGE.into(),
            Dialect::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Dialect::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Dialect::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Dialect::Java => tree_sitter_java::LANGUAGE.into(),
            Dialect::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Dialect::C => tree_sitter_c::LANGUAGE.into(),
            Dialect::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Dialect::Go => tree_sitter_go::LANGUAGE.into(),
            Dialect::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Dialect::Rust => tree_sitter_rust::LANGUAGE.into(),
        }
    }

    /// The query that finds the symbols (`queries/<lang>/symbols.scm`).
    pub(crate) fn symbols_query(self) -> &'static str {
        match self {
            Dialect::Python => include_str!("../queries/python/symbols.scm"),
            Dialect::JavaScript => include_str!("../queries/javascript/symbols.scm"),
            Dialect::TypeScript | Dialect::Tsx => include_str!("../queries/typescript/symbols.scm"),
            Dialect::Java => include_str!("../queries/java/symbols.scm"),
            Dialect::CSharp => include_str!("../queries/csharp/symbols.scm"),
            Dialect::C => include_str!("../queries/c/symbols.scm"),
            Dialect::Cpp => include_str!("../queries/cpp/symbols.scm"),
            Dialect::Go => include_str!("../queries/go/symbols.scm"),
            Dialect::Php => include_str!("../queries/php/symbols.scm"),
            Dialect::Rust => include_str!("../queries/rust/symbols.scm"),
        }
    }
}

/// The grammar for a file, chosen by its extension. `None`: the file is
/// handled as plain text.
pub fn dialect_for_path(path: &Path) -> Option<Dialect> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Dialect::ALL.into_iter().find(|d| d.extensions().contains(&ext.as_str()))
}

/// The language of a file, `Lang::Text` when no grammar handles it.
pub fn lang_for_path(path: &Path) -> Lang {
    dialect_for_path(path).map_or(Lang::Text, Dialect::lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_to_languages() {
        let lang = |p: &str| lang_for_path(Path::new(p));
        assert_eq!(lang("src/main.rs"), Lang::Rust);
        assert_eq!(lang("a/b/App.TSX"), Lang::TypeScript);
        assert_eq!(dialect_for_path(Path::new("App.tsx")), Some(Dialect::Tsx));
        assert_eq!(lang("x.mjs"), Lang::JavaScript);
        assert_eq!(lang("x.hpp"), Lang::Cpp);
        assert_eq!(lang("x.h"), Lang::C);
        assert_eq!(lang("Program.cs"), Lang::CSharp);
        assert_eq!(lang("README.md"), Lang::Text);
        assert_eq!(lang("Makefile"), Lang::Text);
    }

    #[test]
    fn every_extension_belongs_to_one_dialect() {
        let mut seen = std::collections::HashSet::new();
        for dialect in Dialect::ALL {
            for ext in dialect.extensions() {
                assert!(seen.insert(*ext), "{ext} is claimed twice");
            }
        }
    }
}
