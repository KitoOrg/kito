//! What level 2 needs from the syntax of a file: where names are used, and
//! what the file imports.
//!
//! This is not name resolution. It says "the word `getUser` is written
//! here, not after a dot" and "this file takes `getUser` from `./users`";
//! deciding what that means is up to who asks.

use crate::extract::Parsed;
use crate::lang::Dialect;
use std::ops::Range;
use tree_sitter::Node;

/// A name written in a file, outside comments and strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identifier {
    pub range: Range<usize>,
    /// The name is reached through something else: `user.name`, `pkg.Name`,
    /// `self.count`, `ns::item`. It does not, alone, name anything in scope.
    pub member: bool,
    /// For a member, the name right before it when that is a plain name:
    /// the `pkg` of `pkg.Name`.
    pub qualifier: Option<Range<usize>>,
}

/// A name a file takes from a module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedName {
    /// Where the name is written as the module has it.
    pub name: Range<usize>,
    /// The name it has in this file, if another: `import { a as b }`.
    pub alias: Option<Range<usize>>,
}

/// One import of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    /// The module, as written: `./users`, `net/http`, `..models.user`.
    pub module: String,
    pub names: Vec<ImportedName>,
    /// The name the whole module has in this file, when the import gives it
    /// one: `import * as users`, Go's `u "app/users"`, `import a.b as ab`.
    pub namespace: Option<Range<usize>>,
}

/// Kinds of nodes whose second part is reached through the first.
const ACCESSES: &[&str] = &[
    "member_expression",
    "attribute",
    "field_access",
    "field_expression",
    "selector_expression",
    "member_access_expression",
    "member_call_expression",
    "scoped_call_expression",
    "class_constant_access_expression",
    "scoped_identifier",
    "scoped_type_identifier",
    "qualified_identifier",
    "qualified_type",
    "qualified_name",
    "nested_identifier",
    "nested_type_identifier",
];

fn is_name(kind: &str) -> bool {
    kind == "name" || kind.ends_with("identifier") || kind.ends_with("identifier_pattern")
}

fn leaves<'a>(root: Node<'a>, visit: &mut dyn FnMut(Node<'a>)) {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.child_count() == 0 {
            visit(node);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn descendants<'a>(root: Node<'a>, kind: &str, out: &mut Vec<Node<'a>>) {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.kind() == kind {
            out.push(node);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

/// The text of a string literal without its quotes.
fn unquoted(source: &[u8], node: Node<'_>) -> String {
    let text = String::from_utf8_lossy(&source[node.byte_range()]);
    text.trim_matches(|c| c == '"' || c == '\'' || c == '`').to_owned()
}

impl Parsed {
    /// Every name written in the file, in document order.
    pub fn identifiers(&self) -> Vec<Identifier> {
        let mut found = Vec::new();
        leaves(self.root(), &mut |node| {
            let kind = node.kind();
            if !is_name(kind) || node.end_byte() == node.start_byte() {
                return;
            }
            let mut member = matches!(kind, "property_identifier" | "field_identifier");
            let mut qualifier = None;
            if let Some(parent) = node.parent() {
                // A method call written `object.name(...)` in grammars that
                // have no separate node for the access.
                let call_on_object =
                    parent.kind() == "method_invocation" && parent.child_by_field_name("object").is_some();
                if ACCESSES.contains(&parent.kind()) || call_on_object {
                    let first = parent.child(0);
                    let is_first = first.is_some_and(|first| first.id() == node.id());
                    member = member || !is_first;
                    if !is_first {
                        qualifier = first
                            .filter(|f| f.child_count() == 0 && is_name(f.kind()))
                            .map(|f| f.byte_range());
                    }
                }
            }
            found.push(Identifier { range: node.byte_range(), member, qualifier });
        });
        found
    }

    /// The imports of the file, for the languages whose imports name what
    /// they take or give the module a name: JavaScript and TypeScript,
    /// Python, Go. Empty for the others.
    pub fn imports(&self) -> Vec<Import> {
        let source = self.source();
        let root = self.root();
        let mut imports = Vec::new();
        match self.dialect() {
            Dialect::JavaScript | Dialect::TypeScript | Dialect::Tsx => {
                let mut statements = Vec::new();
                descendants(root, "import_statement", &mut statements);
                descendants(root, "export_statement", &mut statements);
                for statement in statements {
                    // `export { a } from "./x"` takes names too; a plain
                    // `export` has no source.
                    let Some(module) = statement.child_by_field_name("source") else { continue };
                    let mut import =
                        Import { module: unquoted(source, module), names: Vec::new(), namespace: None };
                    let mut specifiers = Vec::new();
                    descendants(statement, "import_specifier", &mut specifiers);
                    descendants(statement, "export_specifier", &mut specifiers);
                    for specifier in specifiers {
                        if let Some(name) = specifier.child_by_field_name("name") {
                            import.names.push(ImportedName {
                                name: name.byte_range(),
                                alias: specifier.child_by_field_name("alias").map(|a| a.byte_range()),
                            });
                        }
                    }
                    let mut namespaces = Vec::new();
                    descendants(statement, "namespace_import", &mut namespaces);
                    import.namespace = namespaces
                        .first()
                        .and_then(|n| n.named_child(0))
                        .filter(|n| is_name(n.kind()))
                        .map(|n| n.byte_range());
                    imports.push(import);
                }
            }
            Dialect::Python => {
                let mut statements = Vec::new();
                descendants(root, "import_from_statement", &mut statements);
                for statement in statements {
                    let Some(module) = statement.child_by_field_name("module_name") else { continue };
                    let module = String::from_utf8_lossy(&source[module.byte_range()]).into_owned();
                    let mut import = Import { module, names: Vec::new(), namespace: None };
                    let mut cursor = statement.walk();
                    for name in statement.children_by_field_name("name", &mut cursor) {
                        match name.kind() {
                            "aliased_import" => {
                                if let Some(original) = name.child_by_field_name("name") {
                                    import.names.push(ImportedName {
                                        name: original.byte_range(),
                                        alias: name.child_by_field_name("alias").map(|a| a.byte_range()),
                                    });
                                }
                            }
                            _ => import.names.push(ImportedName { name: name.byte_range(), alias: None }),
                        }
                    }
                    imports.push(import);
                }
                let mut statements = Vec::new();
                descendants(root, "import_statement", &mut statements);
                for statement in statements {
                    let mut cursor = statement.walk();
                    for name in statement.children_by_field_name("name", &mut cursor) {
                        let (module, namespace) = match name.kind() {
                            "aliased_import" => (
                                name.child_by_field_name("name"),
                                name.child_by_field_name("alias").map(|a| a.byte_range()),
                            ),
                            // `import a.b`: the module is used by its full name.
                            _ => (Some(name), (name.named_child_count() <= 1).then(|| name.byte_range())),
                        };
                        if let Some(module) = module {
                            imports.push(Import {
                                module: String::from_utf8_lossy(&source[module.byte_range()]).into_owned(),
                                names: Vec::new(),
                                namespace,
                            });
                        }
                    }
                }
            }
            Dialect::Go => {
                let mut specs = Vec::new();
                descendants(root, "import_spec", &mut specs);
                for spec in specs {
                    let Some(path) = spec.child_by_field_name("path") else { continue };
                    let namespace = spec
                        .child_by_field_name("name")
                        .filter(|name| name.kind() == "package_identifier")
                        .map(|name| name.byte_range());
                    imports.push(Import { module: unquoted(source, path), names: Vec::new(), namespace });
                }
            }
            _ => {}
        }
        imports
    }
}
