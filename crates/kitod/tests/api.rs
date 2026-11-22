//! The daemon's API as it is written down (ADR 0005): `docs/api.md` lists
//! every method the daemon answers, under the permission it needs.

use kitod::plugins::{permission_of, Permission};
use std::collections::BTreeMap;
use std::path::Path;

/// The methods the daemon answers, read from where it tells them apart.
fn answered() -> Vec<String> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server.rs");
    // As it is written, whatever the checkout made of the ends of its lines.
    let text = std::fs::read_to_string(source).unwrap().replace("\r\n", "\n");
    let start = text.find("    fn handle(&self, request: &Request) -> Response {").unwrap();
    let end = start + text[start..].find("\n    }\n").unwrap();
    let mut methods = Vec::new();
    for line in text[start..end].lines() {
        // An arm of the match on the method, at its indentation.
        // A long arm goes on, on the next line, after `| `.
        let Some(arm) =
            line.strip_prefix("                \"").or_else(|| line.strip_prefix("                | \""))
        else {
            continue;
        };
        let names = match arm.split_once(" => ") {
            Some((names, _)) => names,
            None if arm.ends_with('"') => arm,
            None => continue,
        };
        for name in names.split(" | ") {
            let name = name.trim_matches('"');
            if !name.is_empty() && name.bytes().all(|c| c.is_ascii_lowercase() || c == b'_') {
                methods.push(name.to_owned());
            }
        }
    }
    assert!(methods.len() > 40, "the methods of the daemon were not found: {methods:?}");
    methods
}

/// Each method of the document, with the heading it is under.
fn documented() -> BTreeMap<String, String> {
    let doc = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api.md");
    let text = std::fs::read_to_string(doc).unwrap();
    let (mut heading, mut methods) = (String::new(), BTreeMap::new());
    for line in text.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            heading = title.trim_matches('`').to_owned();
        } else if let Some(row) = line.strip_prefix("| `") {
            let name = row.split('`').next().unwrap_or_default();
            // The rows of the table of errors start with a number.
            if !name.starts_with('-') {
                methods.insert(name.to_owned(), heading.clone());
            }
        }
    }
    methods
}

#[test]
fn every_method_is_written_down_under_the_permission_it_needs() {
    let documented = documented();
    let text =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api.md")).unwrap();
    assert!(text.contains(&format!("version **{}**", kitod::rpc::API)), "the document is of another version");
    for method in answered() {
        // Never for a plugin: named in a sentence, in no table.
        if ["plugin_key", "shutdown"].contains(&method.as_str()) {
            assert!(text.contains(&format!("`{method}`")), "{method} is not in docs/api.md");
            continue;
        }
        let heading = documented.get(&method).unwrap_or_else(|| panic!("{method} is not in docs/api.md"));
        let needs = match permission_of(&method) {
            None => "Anybody who reached the daemon",
            Some(Permission::Read) => "read",
            Some(Permission::Write) => "write",
            Some(Permission::Integrate) => "integrate",
            Some(Permission::Admin) => "admin",
            // What a server gives a plugin is no method of a daemon.
            Some(other) => panic!("{method} needs `{}`, which no method of a daemon does", other.word()),
        };
        assert_eq!(heading, needs, "{method} is under `{heading}` in docs/api.md and needs `{needs}`");
    }
    // And nothing is written down that the daemon does not answer.
    let answered = answered();
    for method in documented.keys() {
        assert!(answered.contains(method), "docs/api.md lists `{method}`, which the daemon does not answer");
    }
}
