//! The AST differ: symbol IDs stay stable across edits, renames and moves.

use kito_core::*;
use kito_diff::ast::{diff_symbols, FileChange, Version};
use kito_parse::{dialect_for_path, parse, ParsedFile};
use proptest::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// A set of files and what the history knows about their symbols.
#[derive(Default)]
struct World {
    table: SymbolTable,
    files: BTreeMap<PathBuf, String>,
    blobs: HashMap<BlobId, Vec<u8>>,
}

fn parsed(path: &Path, content: &str) -> ParsedFile {
    parse(dialect_for_path(path).expect("a known language"), content.as_bytes()).expect("a tree")
}

impl World {
    /// Changes some files (`None` removes one), runs the differ, applies the
    /// ops to the table and describes them, one line per op.
    fn step(&mut self, changes: &[(&str, Option<&str>)]) -> Vec<String> {
        self.step_with(changes, true)
    }

    fn step_with(&mut self, changes: &[(&str, Option<&str>)], old_is_readable: bool) -> Vec<String> {
        let paths: Vec<PathBuf> = changes.iter().map(|(p, _)| PathBuf::from(p)).collect();
        let old: Vec<Option<(String, ParsedFile)>> = paths
            .iter()
            .map(|p| self.files.get(p).filter(|_| old_is_readable).map(|c| (c.clone(), parsed(p, c))))
            .collect();
        let new: Vec<Option<(String, ParsedFile)>> =
            changes.iter().zip(&paths).map(|((_, c), p)| c.map(|c| (c.to_owned(), parsed(p, c)))).collect();
        let file_changes: Vec<FileChange<'_>> = paths
            .iter()
            .zip(old.iter().zip(&new))
            .map(|(path, (old, new))| FileChange {
                path,
                old: old.as_ref().map(|(c, p)| Version { content: c.as_bytes(), parsed: p }),
                new: new.as_ref().map(|(c, p)| Version { content: c.as_bytes(), parsed: p }),
            })
            .collect();

        let result = diff_symbols(&self.table, LaneId::MAIN, &file_changes);
        for blob in &result.blobs {
            self.blobs.insert(BlobId::of(blob), blob.clone());
        }
        let mut described = Vec::new();
        for op in &result.ops {
            // Names are read before removals and after everything else.
            let before = op.symbol().and_then(|id| self.table.qualified_name(id));
            self.table.apply(op).unwrap_or_else(|e| panic!("{op:?} does not apply: {e}"));
            let after = op.symbol().and_then(|id| self.table.qualified_name(id));
            let name = after.or(before).unwrap_or_default();
            for blob in op.blobs() {
                assert!(
                    self.blobs.contains_key(&blob),
                    "{} cites a blob the differ did not return",
                    op.name()
                );
            }
            described.push(match op {
                OpKind::AddSymbol { sym, .. } => format!("Add {:?} {name}", sym.kind),
                OpKind::RemoveSymbol { .. } => format!("Remove {name}"),
                OpKind::Rename { from, to, .. } => format!("Rename {from} -> {to}"),
                OpKind::Move { from, to, .. } => {
                    format!("Move {name}: {} -> {}", from.file.display(), to.file.display())
                }
                OpKind::ModifyBody { before, after, edits, .. } => {
                    if !edits.is_empty() {
                        assert_eq!(&apply_edits(&self.blobs[before], edits).unwrap(), &self.blobs[after]);
                    }
                    format!("ModifyBody {name}")
                }
                OpKind::ModifySignature { .. } => format!("ModifySignature {name}"),
                other => panic!("not a symbolic op: {other:?}"),
            });
        }
        for ((path, _), content) in changes.iter().zip(changes.iter().map(|(_, c)| c)) {
            match content {
                Some(content) => self.files.insert(path.into(), (*content).to_owned()),
                None => self.files.remove(Path::new(path)),
            };
        }
        // The file-level ops of the batch do not touch the symbols: whatever
        // lived in a removed file was removed or moved by the differ.
        for (path, content) in changes {
            if content.is_none() {
                self.table.apply(&OpKind::FileRemove { path: path.into() }).unwrap();
            }
        }
        self.assert_table_mirrors_files();
        described
    }

    /// After every step the table lists exactly the symbols the parser sees.
    fn assert_table_mirrors_files(&self) {
        for (path, content) in &self.files {
            let file = parsed(path, content);
            let mut seen: Vec<(String, SymbolKind)> =
                (0..file.symbols.len()).map(|i| (file.qualified_name(i), file.symbols[i].kind)).collect();
            let mut known: Vec<(String, SymbolKind)> = self
                .table
                .in_file(path)
                .map(|(id, e)| (self.table.qualified_name(id).unwrap(), e.kind))
                .collect();
            seen.sort();
            known.sort();
            assert_eq!(known, seen, "{}", path.display());
        }
    }

    fn id(&self, file: &str, name: &str) -> SymbolId {
        match self.table.find(Path::new(file), name).as_slice() {
            [id] => *id,
            other => panic!("{name} in {file}: {other:?}"),
        }
    }
}

const AUTH: &str = r#"
import { db } from "./db";

export function getUser(id: string): User {
  const cached = cache.get(id);
  if (cached) {
    return cached;
  }
  const user = db.find(id);
  cache.set(id, user);
  return user;
}

export class Session {
  constructor(private user: User) {}

  isValid(now: number): boolean {
    return this.user.expires > now && !this.user.revoked;
  }

  refresh(now: number): void {
    this.user.expires = now + SESSION_LENGTH;
    audit.log("refresh", this.user.id);
  }
}
"#;

#[test]
fn a_new_file_adds_its_symbols_with_their_nesting() {
    let mut w = World::default();
    let ops = w.step(&[("src/auth.ts", Some(AUTH))]);
    assert_eq!(
        ops,
        [
            "Add Function getUser",
            "Add Class Session",
            "Add Method Session.constructor",
            "Add Method Session.isValid",
            "Add Method Session.refresh",
        ]
    );
    let method = w.table.get(w.id("src/auth.ts", "Session.refresh")).unwrap();
    assert_eq!(method.parent, Some(w.id("src/auth.ts", "Session")));
    assert_eq!((method.lang, method.file.as_path()), (Lang::TypeScript, Path::new("src/auth.ts")));
}

#[test]
fn reformatting_produces_no_operation() {
    let mut w = World::default();
    w.step(&[("src/auth.ts", Some(AUTH))]);
    let reformatted = AUTH
        .replace(
            "  const cached = cache.get(id);",
            "  // look in the cache first\n  const cached =\n      cache.get( id );",
        )
        .replace("export class Session {", "/** A login session. */\nexport class Session\n{")
        .replace("\n\n", "\n\n\n");
    assert_ne!(reformatted, AUTH);
    assert!(w.step(&[("src/auth.ts", Some(&reformatted))]).is_empty());
}

#[test]
fn edits_touch_only_the_symbol_that_changed() {
    let mut w = World::default();
    w.step(&[("src/auth.ts", Some(AUTH))]);
    let ids = (w.id("src/auth.ts", "getUser"), w.id("src/auth.ts", "Session.refresh"));

    let body = AUTH.replace("const user = db.find(id);", "const user = await db.find(id);");
    assert_eq!(w.step(&[("src/auth.ts", Some(&body))]), ["ModifyBody getUser"]);

    let signature = body.replace("refresh(now: number): void", "refresh(now: number, force = false): void");
    assert_eq!(w.step(&[("src/auth.ts", Some(&signature))]), ["ModifySignature Session.refresh"]);

    let both = signature.replace(
        "isValid(now: number): boolean {\n    return",
        "isValid(at: number): boolean {\n    return at > 0 &&",
    );
    assert_eq!(
        w.step(&[("src/auth.ts", Some(&both))]),
        ["ModifySignature Session.isValid", "ModifyBody Session.isValid"]
    );

    // A field is part of the class itself, not of any method.
    let field = both.replace(
        "constructor(private user: User) {}",
        "private renewals = 0;\n  constructor(private user: User) {}",
    );
    assert_eq!(w.step(&[("src/auth.ts", Some(&field))]), ["ModifyBody Session"]);
    assert_eq!(ids, (w.id("src/auth.ts", "getUser"), w.id("src/auth.ts", "Session.refresh")));
}

#[test]
fn renames_keep_the_id() {
    let mut w = World::default();
    w.step(&[("src/auth.ts", Some(AUTH))]);
    let (function, class, method) = (
        w.id("src/auth.ts", "getUser"),
        w.id("src/auth.ts", "Session"),
        w.id("src/auth.ts", "Session.isValid"),
    );

    // Same body, new name.
    let renamed = AUTH.replace("getUser", "fetchUser");
    assert_eq!(w.step(&[("src/auth.ts", Some(&renamed))]), ["Rename getUser -> fetchUser"]);
    assert_eq!(w.id("src/auth.ts", "fetchUser"), function);

    // New name and a small edit in the same save.
    let again =
        renamed.replace("fetchUser", "loadUser").replace("cache.set(id, user);", "cache.set(id, user, TTL);");
    assert_eq!(
        w.step(&[("src/auth.ts", Some(&again))]),
        ["Rename fetchUser -> loadUser", "ModifyBody loadUser"]
    );
    assert_eq!(w.id("src/auth.ts", "loadUser"), function);

    // A class with almost no text of its own is recognized by its members,
    // which keep their IDs without any operation.
    let class_renamed = again.replace("class Session", "class LoginSession");
    assert_eq!(w.step(&[("src/auth.ts", Some(&class_renamed))]), ["Rename Session -> LoginSession"]);
    assert_eq!(w.id("src/auth.ts", "LoginSession"), class);
    assert_eq!(w.id("src/auth.ts", "LoginSession.isValid"), method);

    // A method renamed inside it.
    let method_renamed = class_renamed.replace("isValid(", "stillValid(");
    assert_eq!(w.step(&[("src/auth.ts", Some(&method_renamed))]), ["Rename isValid -> stillValid"]);
    assert_eq!(w.id("src/auth.ts", "LoginSession.stillValid"), method);
}

#[test]
fn look_alikes_are_not_mistaken_for_renames() {
    let mut w = World::default();
    // Tiny bodies say nothing about identity.
    w.step(&[("a.ts", Some("export function one() { return 1; }\n"))]);
    assert_eq!(
        w.step(&[("a.ts", Some("export function uno() { return 1; }\n"))]),
        ["Remove one", "Add Function uno"]
    );

    // A different function that merely has the same signature.
    let before = "export function total(items: Item[]): number {\n  let sum = 0;\n  for (const item of items) {\n    sum += item.price * item.quantity;\n  }\n  return sum;\n}\n";
    let after = "export function names(items: Item[]): number {\n  const seen = new Set<string>();\n  items.forEach((item) => seen.add(item.name.toLowerCase()));\n  return seen.size;\n}\n";
    let mut w = World::default();
    w.step(&[("b.ts", Some(before))]);
    assert_eq!(w.step(&[("b.ts", Some(after))]), ["Remove total", "Add Function names"]);

    // Two identical functions renamed at once: ambiguous by body, but each
    // still pairs with one and keeps an ID.
    let twin = |a: &str, b: &str| {
        let body =
            "(x: number): number {\n  const y = x * 2;\n  log(\"double\", x, y);\n  return y + OFFSET;\n}\n";
        format!("function {a}{body}function {b}{body}")
    };
    let mut w = World::default();
    w.step(&[("c.ts", Some(&twin("first", "second")))]);
    let ids = (w.id("c.ts", "first"), w.id("c.ts", "second"));
    assert_eq!(
        w.step(&[("c.ts", Some(&twin("primo", "secondo")))]),
        ["Rename first -> primo", "Rename second -> secondo"]
    );
    assert_eq!(ids, (w.id("c.ts", "primo"), w.id("c.ts", "secondo")));
}

#[test]
fn a_function_moved_to_another_file_keeps_its_id_and_history() {
    let mut w = World::default();
    w.step(&[("src/auth.ts", Some(AUTH)), ("src/users.ts", Some("export const USERS = 1;\n"))]);
    let id = w.id("src/auth.ts", "getUser");

    // Step 1: rename.
    let renamed = AUTH.replace("getUser", "fetchUser");
    assert_eq!(w.step(&[("src/auth.ts", Some(&renamed))]), ["Rename getUser -> fetchUser"]);

    // Step 2: cut from one file, pasted into another, in the same save.
    let start = renamed.find("export function fetchUser").unwrap();
    let end = renamed.find("export class Session").unwrap();
    let function = &renamed[start..end];
    let without = format!("{}{}", &renamed[..start], &renamed[end..]);
    let with = format!("export const USERS = 1;\n\n{function}");
    let ops = w.step(&[("src/auth.ts", Some(&without)), ("src/users.ts", Some(&with))]);
    assert_eq!(ops, ["Move fetchUser: src/auth.ts -> src/users.ts"]);
    assert_eq!(w.id("src/users.ts", "fetchUser"), id);
    assert!(w.table.find(Path::new("src/auth.ts"), "fetchUser").is_empty());

    // Step 3: moved again, renamed and edited on the way, and the old file deleted.
    let moved = function.replace("fetchUser", "loadUser");
    let ops = w.step(&[("src/users.ts", None), ("src/people.ts", Some(&moved))]);
    assert_eq!(
        ops,
        ["Rename fetchUser -> loadUser", "Move loadUser: src/users.ts -> src/people.ts", "Remove USERS"]
    );
    assert_eq!(w.id("src/people.ts", "loadUser"), id);
}

#[test]
fn a_whole_class_moves_with_its_methods() {
    let mut w = World::default();
    w.step(&[("src/auth.ts", Some(AUTH))]);
    let (class, method) = (w.id("src/auth.ts", "Session"), w.id("src/auth.ts", "Session.refresh"));
    let start = AUTH.find("export class Session").unwrap();
    let ops = w.step(&[("src/auth.ts", Some(&AUTH[..start])), ("src/session.ts", Some(&AUTH[start..]))]);
    assert_eq!(ops, ["Move Session: src/auth.ts -> src/session.ts"]);
    assert_eq!(
        (w.id("src/session.ts", "Session"), w.id("src/session.ts", "Session.refresh")),
        (class, method)
    );
}

#[test]
fn additions_removals_and_overloads() {
    let mut w = World::default();
    let v1 = "function f(a: string): void;\nfunction f(a: number): void;\nfunction f(a: any) { console.log(a); }\n";
    w.step(&[("o.ts", Some(v1))]);
    let ids: Vec<SymbolId> = w.table.find(Path::new("o.ts"), "f");
    assert_eq!(ids.len(), 3);

    // A new overload in front: the existing ones keep their IDs, in order.
    let v2 = format!("function f(a: boolean): void;\n{v1}");
    assert_eq!(w.step(&[("o.ts", Some(&v2))]), ["Add Function f"]);
    assert_eq!(&w.table.find(Path::new("o.ts"), "f")[..3], &ids[..]);

    // Removing a class removes its members first.
    let mut w = World::default();
    w.step(&[("src/auth.ts", Some(AUTH))]);
    let start = AUTH.find("export class Session").unwrap();
    assert_eq!(
        w.step(&[("src/auth.ts", Some(&AUTH[..start]))]),
        ["Remove Session.constructor", "Remove Session.isValid", "Remove Session.refresh", "Remove Session"]
    );
    // IDs are never reused: a class with the same name later is a new symbol.
    let gone = w.table.next_seq(LaneId::MAIN);
    w.step(&[("src/auth.ts", Some(AUTH))]);
    assert_eq!(w.id("src/auth.ts", "Session").seq(), gone);
}

#[test]
fn identity_survives_an_unreadable_old_version() {
    let mut w = World::default();
    w.step(&[("src/auth.ts", Some(AUTH))]);
    let ids = (w.id("src/auth.ts", "getUser"), w.id("src/auth.ts", "Session.isValid"));
    // The old content cannot be parsed (it was saved half-written): the
    // history still knows the names, so nothing is lost and nothing invented.
    let edited = AUTH.replace("const user = db.find(id);", "const user = db.findOne(id);");
    assert!(w.step_with(&[("src/auth.ts", Some(&edited))], false).is_empty());
    assert_eq!(ids, (w.id("src/auth.ts", "getUser"), w.id("src/auth.ts", "Session.isValid")));
    // What really disappeared is still removed.
    let start = edited.find("export class Session").unwrap();
    let ops = w.step_with(&[("src/auth.ts", Some(&edited[..start]))], false);
    assert_eq!(ops.len(), 4);
    assert!(ops.iter().all(|op| op.starts_with("Remove Session")));
}

#[test]
fn a_file_the_history_never_analyzed_is_indexed_on_first_touch() {
    let mut w = World::default();
    // The file exists (imported as text) but has no symbols in the table.
    w.files.insert("src/auth.ts".into(), AUTH.to_owned());
    let edited = AUTH.replace("getUser", "fetchUser");
    let ops = w.step(&[("src/auth.ts", Some(&edited))]);
    assert_eq!(ops.len(), 5);
    assert!(ops.iter().all(|op| op.starts_with("Add ")));
    assert_eq!(ops[0], "Add Function fetchUser");
}

#[test]
fn other_languages_follow_the_same_rules() {
    let mut w = World::default();
    let go = "package demo\n\ntype Server struct {\n\tport int\n}\n\nfunc (s *Server) Start() error {\n\tfmt.Println(\"starting\", s.port)\n\tif s.port == 0 {\n\t\treturn errNoPort\n\t}\n\treturn listen(s.port)\n}\n";
    w.step(&[("server.go", Some(go))]);
    let id = w.id("server.go", "Server.Start");
    assert_eq!(
        w.step(&[("server.go", Some(&go.replace(") Start()", ") Run()")))]),
        ["Rename Server.Start -> Server.Run"]
    );
    assert_eq!(w.id("server.go", "Server.Run"), id);

    let py = "import os\n\n@cached\ndef load(path):\n    with open(path) as handle:\n        data = handle.read()\n    return parse(data, strict=True)\n\nclass Loader:\n    def run(self, path):\n        return load(os.path.join(self.root, path))\n";
    w.step(&[("loader.py", Some(py))]);
    let id = w.id("loader.py", "load");
    let edited = py
        .replace("def load(path):", "def read_config(path):")
        .replace("load(os.path", "read_config(os.path");
    assert_eq!(
        w.step(&[("loader.py", Some(&edited))]),
        ["Rename load -> read_config", "ModifyBody Loader.run"]
    );
    assert_eq!(w.id("loader.py", "read_config"), id);

    let rs = "pub struct Counter { n: u64 }\n\nimpl Counter {\n    pub fn bump(&mut self, by: u64) -> u64 {\n        self.n = self.n.saturating_add(by);\n        log::debug!(\"bumped to {}\", self.n);\n        self.n\n    }\n}\n";
    w.step(&[("counter.rs", Some(rs))]);
    let id = w.id("counter.rs", "Counter.bump");
    assert_eq!(w.step(&[("counter.rs", Some(&rs.replace("fn bump", "fn add")))]), ["Rename bump -> add"]);
    assert_eq!(w.id("counter.rs", "Counter.add"), id);
}

/// A generated function: its name and body are independent, so the test
/// knows which function is which whatever the differ decides.
fn function(name: &str, flavor: u8, tweak: u8) -> String {
    let body = match flavor % 4 {
        0 => format!("  const total = items.reduce((sum, item) => sum + item.price * {}, 0);\n  audit.record(\"total\", total);\n  return total;", 1 + tweak),
        1 => format!("  for (const user of users) {{\n    if (user.age > {}) {{ notify(user.email, template); }}\n  }}\n  return users.length;", 18 + tweak),
        2 => format!("  const response = await fetch(endpoint + path, {{ retries: {} }});\n  if (!response.ok) {{ throw new HttpError(response.status); }}\n  return response.json();", 3 + tweak),
        _ => format!("  const parts = input.trim().split(separator).filter(Boolean);\n  cache.set(key, parts.slice(0, {}));\n  return parts.map(normalize).join(\"/\");", 5 + tweak),
    };
    format!("export function {name}(a: A, b: B) {{\n{body}\n}}\n\n")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Whatever the sequence of edits, the ops always apply, the table always
    /// mirrors the files, and a function that was only renamed, or only
    /// edited a little, keeps its ID.
    #[test]
    fn ids_follow_functions_through_random_edits(
        edits in prop::collection::vec((0usize..4, 0u8..4, any::<bool>()), 1..12),
    ) {
        let mut w = World::default();
        // Four functions with distinct bodies; `names[i]` and `tweaks[i]` change over time.
        let mut names: Vec<String> = (0..4).map(|i| format!("fn{i}")).collect();
        let mut tweaks = [0u8; 4];
        let render = |names: &[String], tweaks: &[u8; 4]| -> String {
            (0..4).map(|i| function(&names[i], i as u8, tweaks[i])).collect()
        };
        w.step(&[("m.ts", Some(&render(&names, &tweaks)))]);
        let ids: Vec<SymbolId> = names.iter().map(|n| w.id("m.ts", n)).collect();

        for (step, (which, tweak, rename)) in edits.into_iter().enumerate() {
            if rename {
                names[which] = format!("renamed{step}");
            } else {
                tweaks[which] = tweak;
            }
            w.step(&[("m.ts", Some(&render(&names, &tweaks)))]);
            for i in 0..4 {
                prop_assert_eq!(w.id("m.ts", &names[i]), ids[i], "function {} lost its identity at step {}", i, step);
            }
        }
    }
}
