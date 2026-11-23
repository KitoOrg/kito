//! The composition rules of the semantic rebase, one scenario each.

mod common;

use common::*;
use kito_core::*;
use kito_merge::{Choice, Subject};
use proptest::prelude::*;

const SHOP: &str = r#"import { db } from "./db";

// Prices are in cents.
export function total(items: Item[]): number {
  let sum = 0;
  for (const item of items) {
    sum += item.price * item.quantity;
  }
  return sum;
}

export function discount(amount: number, percent: number): number {
  const cut = Math.round((amount * percent) / 100);
  return amount - cut;
}

export class Cart {
  private items: Item[] = [];

  add(item: Item): void {
    this.items.push(item);
    audit.log("add", item.id);
  }

  clear(): void {
    this.items = [];
    audit.log("clear");
  }
}

export function receipt(cart: Cart): string {
  const lines = cart.list().map((item) => `${item.name} x ${item.quantity}`);
  lines.push(`total ${total(cart.list())}`);
  return lines.join("\n");
}
"#;

fn shop() -> World {
    World::new(&[("src/shop.ts", SHOP), ("README.md", "# Shop\n\nA demo.\n")])
}

#[test]
fn changes_to_different_symbols_of_a_file_both_apply() {
    let mut w = shop();
    let lane = w.open();
    // The main lane edits one function, the lane the next one, and a method.
    let on_main = SHOP.replace("let sum = 0;", "let sum = 0; // cents");
    let on_lane = SHOP
        .replace(
            "const cut = Math.round((amount * percent) / 100);",
            "const cut = Math.floor((amount * percent) / 100);",
        )
        .replace("audit.log(\"clear\");", "audit.log(\"clear\", this.items.length);");
    w.on_main(&[("src/shop.ts", Some(&on_main))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&on_lane))]);

    let outcome = w.integrate(lane);
    assert!(outcome.merged_symbols.is_empty(), "no symbol was changed on both sides");
    let merged = w.main_file("src/shop.ts");
    for needle in ["let sum = 0; // cents", "Math.floor(", "audit.log(\"clear\", this.items.length);"] {
        assert!(merged.contains(needle), "{needle} is missing from:\n{merged}");
    }
    // Nothing else moved.
    assert_eq!(merged.lines().count(), SHOP.lines().count());
}

#[test]
fn two_sides_adding_at_the_same_place_do_not_conflict() {
    let mut w = shop();
    let (a, b) = (w.open(), w.open());
    // Both append a function at the end of the file, and a method at the
    // end of the class: for a line merge, the same lines changed twice.
    let add = |name: &str| {
        SHOP.replace(
            "    audit.log(\"clear\");\n  }\n",
            &format!("    audit.log(\"clear\");\n  }}\n\n  {name}Count(): number {{\n    return this.items.length;\n  }}\n"),
        ) + &format!("\nexport function {name}(): string {{\n  return \"{name}\";\n}}\n")
    };
    w.on_lane(a, &[("src/shop.ts", Some(&add("alpha")))]);
    w.on_lane(b, &[("src/shop.ts", Some(&add("beta")))]);
    let (alpha, beta) = (w.lanes[a].0.id("src/shop.ts", "alpha"), w.lanes[b].0.id("src/shop.ts", "beta"));

    w.integrate(a);
    w.integrate(b);
    let merged = w.main_file("src/shop.ts");
    for needle in ["function alpha()", "function beta()", "alphaCount()", "betaCount()"] {
        assert_eq!(merged.matches(needle).count(), 1, "{needle} in:\n{merged}");
    }
    // The symbols are the ones the lanes created, not copies.
    assert_eq!((w.main.id("src/shop.ts", "alpha"), w.main.id("src/shop.ts", "beta")), (alpha, beta));
    assert_eq!((alpha.origin(), beta.origin()), (LaneId(1), LaneId(2)));

    // The other order gives the same file.
    let mut again = shop();
    let (a, b) = (again.open(), again.open());
    again.on_lane(a, &[("src/shop.ts", Some(&add("alpha")))]);
    again.on_lane(b, &[("src/shop.ts", Some(&add("beta")))]);
    again.integrate(b);
    again.integrate(a);
    assert_eq!(again.main_file("src/shop.ts"), merged);
}

#[test]
fn the_same_symbol_changed_twice_merges_line_by_line_or_conflicts() {
    let mut w = shop();
    let lane = w.open();
    let id = w.main.id("src/shop.ts", "total");
    // Different lines of the same function.
    w.on_main(&[("src/shop.ts", Some(&SHOP.replace("let sum = 0;", "let sum = 0; // cents")))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&SHOP.replace("  return sum;", "  return Math.max(sum, 0);")))]);
    let outcome = w.integrate(lane);
    assert_eq!(outcome.merged_symbols, [id]);
    let merged = w.main_file("src/shop.ts");
    assert!(
        merged.contains("let sum = 0; // cents") && merged.contains("return Math.max(sum, 0);"),
        "{merged}"
    );

    // The same line, two ways.
    let lane = w.open();
    w.on_main(&[(
        "src/shop.ts",
        Some(&merged.replace("return Math.max(sum, 0);", "return Math.max(sum, 1);")),
    )]);
    w.on_lane(
        lane,
        &[("src/shop.ts", Some(&merged.replace("return Math.max(sum, 0);", "return Math.abs(sum);")))],
    );
    let outcome = w.merge(lane);
    assert_eq!(describe(&outcome), ["BodyBody total in src/shop.ts"]);
    let conflict = &outcome.conflicts[0];
    assert_eq!(conflict.subject, Subject::Symbol(id));
    let text = |t: &Option<Vec<u8>>| String::from_utf8(t.clone().unwrap()).unwrap();
    assert!(text(&conflict.base).contains("Math.max(sum, 0)"));
    assert!(text(&conflict.main).contains("Math.max(sum, 1)"));
    assert!(text(&conflict.lane).contains("Math.abs(sum)"));
    assert!(
        text(&conflict.lane).starts_with("export function total")
            && !text(&conflict.lane).contains("discount")
    );

    // Each way of deciding it.
    let main_wins = w.merge_with(lane, &[(Subject::Symbol(id), Choice::Main)]);
    assert!(main_wins.conflicts.is_empty() && main_wins.files.is_empty(), "nothing changes on the main lane");
    let lane_wins = w.merge_with(lane, &[(Subject::Symbol(id), Choice::Lane)]);
    let content =
        String::from_utf8(lane_wins.files[std::path::Path::new("src/shop.ts")].clone().unwrap().content)
            .unwrap();
    assert!(content.contains("Math.abs(sum)") && !content.contains("Math.max(sum, 1)"));
    let written = "export function total(items: Item[]): number {\n  return 42;\n}";
    w.integrate_with(lane, &[(Subject::Symbol(id), Choice::Text(written.into()))]);
    let merged = w.main_file("src/shop.ts");
    assert!(merged.contains("return 42;\n}\n\nexport function discount"), "{merged}");
    assert_eq!(w.main.id("src/shop.ts", "total"), id);
}

#[test]
fn renames_compose_with_edits_and_clash_with_other_renames() {
    let mut w = World::new(&[(
        "tree.ts",
        "export function walk(node: Node, depth: number): number {\n  if (!node.next) {\n    return depth;\n  }\n  return walk(node.next, depth + 1);\n}\n",
    )]);
    let base = w.main_file("tree.ts");
    let id = w.main.id("tree.ts", "walk");
    let (edit, rename) = (w.open(), w.open());
    // One lane renames the (recursive) function, the other edits the very
    // line of the recursive call.
    w.on_lane(rename, &[("tree.ts", Some(&base.replace("walk", "descend")))]);
    w.on_lane(edit, &[("tree.ts", Some(&base.replace("depth + 1);", "depth + 1) + 0;")))]);
    w.integrate(rename);
    w.integrate(edit);
    assert_eq!(
        w.main_file("tree.ts"),
        base.replace("walk", "descend").replace("depth + 1);", "depth + 1) + 0;")
    );
    assert_eq!(w.main.id("tree.ts", "descend"), id);

    // Two different new names.
    let (one, two) = (w.open(), w.open());
    let now = w.main_file("tree.ts");
    w.on_lane(one, &[("tree.ts", Some(&now.replace("descend", "goDown")))]);
    w.on_lane(two, &[("tree.ts", Some(&now.replace("descend", "sink")))]);
    w.integrate(one);
    let outcome = w.merge(two);
    assert_eq!(describe(&outcome), ["RenameRename goDown in tree.ts"]);
    w.integrate_with(two, &[(Subject::Symbol(id), Choice::Lane)]);
    assert_eq!(w.main.id("tree.ts", "sink"), id);
    assert!(!w.main_file("tree.ts").contains("goDown"));
}

#[test]
fn removing_what_the_other_side_changed_is_a_conflict() {
    let mut w = shop();
    let id = w.main.id("src/shop.ts", "discount");
    let without = {
        let start = SHOP.find("export function discount").unwrap();
        let end = SHOP.find("export class Cart").unwrap();
        format!("{}{}", &SHOP[..start], &SHOP[end..])
    };
    let edited = SHOP.replace("return amount - cut;", "return Math.max(amount - cut, 0);");

    // Removed on the lane, changed on the main lane.
    let lane = w.open();
    w.on_main(&[("src/shop.ts", Some(&edited))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&without))]);
    let outcome = w.merge(lane);
    assert_eq!(describe(&outcome), ["RemoveModify discount in src/shop.ts"]);
    assert!(outcome.conflicts[0].lane.is_none() && outcome.conflicts[0].main.is_some());
    let kept = w.merge_with(lane, &[(Subject::Symbol(id), Choice::Main)]);
    assert!(kept.conflicts.is_empty() && kept.files.is_empty());
    w.integrate_with(lane, &[(Subject::Symbol(id), Choice::Lane)]);
    assert_eq!(w.main_file("src/shop.ts"), without);
    assert!(w.main.table.get(id).is_none());

    // Removed on the main lane, changed on the lane.
    let mut w = shop();
    let lane = w.open();
    w.on_main(&[("src/shop.ts", Some(&without))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&edited))]);
    assert_eq!(describe(&w.merge(lane)), ["RemoveModify discount in src/shop.ts"]);
    let dropped = w.merge_with(lane, &[(Subject::Symbol(id), Choice::Main)]);
    assert!(dropped.conflicts.is_empty() && dropped.files.is_empty());
    w.integrate_with(lane, &[(Subject::Symbol(id), Choice::Lane)]);
    assert!(w.main_file("src/shop.ts").contains("return Math.max(amount - cut, 0);"));
    assert_eq!(w.main.id("src/shop.ts", "discount"), id);

    // Removed on one side and left alone on the other: just removed.
    let mut w = shop();
    let lane = w.open();
    w.on_main(&[("src/shop.ts", Some(&SHOP.replace("let sum = 0;", "let sum = 0; // cents")))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&without))]);
    w.integrate(lane);
    let merged = w.main_file("src/shop.ts");
    assert!(!merged.contains("discount") && merged.contains("// cents"), "{merged}");
}

#[test]
fn a_symbol_moved_on_one_side_takes_the_other_sides_changes_along() {
    let cut = |text: &str| {
        let start = text.find("export function discount").unwrap();
        let end = text.find("export class Cart").unwrap();
        (text[start..end].to_owned(), format!("{}{}", &text[..start], &text[end..]))
    };
    let edited = SHOP.replace("return amount - cut;", "return Math.max(amount - cut, 0);");

    // Moved to another file on the lane, edited on the main lane.
    let mut w = shop();
    let id = w.main.id("src/shop.ts", "discount");
    let lane = w.open();
    let (function, rest) = cut(SHOP);
    w.on_main(&[("src/shop.ts", Some(&edited))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&rest)), ("src/prices.ts", Some(&function))]);
    w.integrate(lane);
    assert!(!w.main_file("src/shop.ts").contains("discount"));
    assert!(w.main_file("src/prices.ts").contains("return Math.max(amount - cut, 0);"));
    assert_eq!(w.main.id("src/prices.ts", "discount"), id, "the ID follows the symbol");

    // Moved on the main lane, edited on the lane.
    let mut w = shop();
    let lane = w.open();
    w.on_main(&[("src/shop.ts", Some(&rest)), ("src/prices.ts", Some(&function))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&edited))]);
    w.integrate(lane);
    assert!(!w.main_file("src/shop.ts").contains("discount"));
    assert!(w.main_file("src/prices.ts").contains("return Math.max(amount - cut, 0);"));
    assert_eq!(w.main.id("src/prices.ts", "discount"), id);

    // A method that changes class on the lane while the main lane edits it.
    let mut w = shop();
    let method = w.main.id("src/shop.ts", "Cart.clear");
    let lane = w.open();
    let clear = "  clear(): void {\n    this.items = [];\n    audit.log(\"clear\");\n  }\n";
    let moved = SHOP.replace(&format!("\n{clear}"), "") + &format!("\nexport class Trash {{\n{clear}}}\n");
    w.on_main(&[("src/shop.ts", Some(&SHOP.replace("audit.log(\"clear\");", "audit.log(\"cleared\");")))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&moved))]);
    w.integrate(lane);
    let merged = w.main_file("src/shop.ts");
    assert!(
        merged.contains(
            "export class Trash {\n  clear(): void {\n    this.items = [];\n    audit.log(\"cleared\");"
        ),
        "{merged}"
    );
    assert_eq!(merged.matches("clear(): void").count(), 1);
    assert_eq!(w.main.id("src/shop.ts", "Trash.clear"), method);

    // A function that goes inside a namespace on the lane (every line gets
    // more indentation) while the main lane edits one of its lines.
    let mut w = shop();
    let id = w.main.id("src/shop.ts", "discount");
    let lane = w.open();
    let (function, rest) = cut(SHOP);
    let indented: String = function.trim_end().lines().map(|l| format!("  {l}\n")).collect();
    let nested = format!("{rest}\nexport namespace Prices {{\n{indented}}}\n");
    w.on_main(&[("src/shop.ts", Some(&edited))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&nested))]);
    w.integrate(lane);
    let merged = w.main_file("src/shop.ts");
    assert!(
        merged.contains("export namespace Prices {\n  export function discount(amount: number, percent: number): number {\n    const cut = Math.round((amount * percent) / 100);\n    return Math.max(amount - cut, 0);\n  }\n}\n"),
        "{merged}"
    );
    assert_eq!(w.main.id("src/shop.ts", "Prices.discount"), id);
}

#[test]
fn text_outside_the_symbols_and_files_without_a_grammar_merge_as_text() {
    let mut w = shop();
    let lane = w.open();
    // An import on one side, the comment above a function on the other.
    w.on_main(&[
        (
            "src/shop.ts",
            Some(&SHOP.replace(
                "import { db } from \"./db\";",
                "import { db } from \"./db\";\nimport { audit } from \"./audit\";",
            )),
        ),
        ("README.md", Some("# Shop\n\nA demo.\n\n## Usage\n")),
    ]);
    w.on_lane(
        lane,
        &[
            (
                "src/shop.ts",
                Some(&SHOP.replace("// Prices are in cents.", "// Prices are in cents, never negative.")),
            ),
            ("README.md", Some("# The Shop\n\nA demo.\n")),
        ],
    );
    w.integrate(lane);
    let merged = w.main_file("src/shop.ts");
    assert!(merged.contains("import { audit }") && merged.contains("never negative"), "{merged}");
    assert_eq!(w.main_file("README.md"), "# The Shop\n\nA demo.\n\n## Usage\n");

    // The same line of a text file, two ways.
    let lane = w.open();
    w.on_main(&[("README.md", Some("# The Shop\n\nA store.\n\n## Usage\n"))]);
    w.on_lane(lane, &[("README.md", Some("# The Shop\n\nAn example.\n\n## Usage\n"))]);
    assert_eq!(describe(&w.merge(lane)), ["Text README.md in README.md"]);
    let subject = Subject::File("README.md".into());
    w.integrate_with(lane, &[(subject, Choice::Text(b"# The Shop\n\nA store, as an example.\n".to_vec()))]);
    assert_eq!(w.main_file("README.md"), "# The Shop\n\nA store, as an example.\n");
}

#[test]
fn renamed_and_removed_files() {
    // Renamed on the lane (unchanged), edited on the main lane: the edit
    // follows the file, and so do its symbols.
    let mut w = shop();
    let id = w.main.id("src/shop.ts", "total");
    let lane = w.open();
    let edited = SHOP.replace("let sum = 0;", "let sum = 0; // cents");
    w.on_main(&[("src/shop.ts", Some(&edited))]);
    w.on_lane(lane, &[("src/shop.ts", None), ("src/store.ts", Some(SHOP))]);
    w.integrate(lane);
    assert_eq!(w.main_file("src/store.ts"), edited);
    assert!(w.main.read(&w.blobs, "src/shop.ts").is_none());
    assert_eq!(w.main.id("src/store.ts", "total"), id);

    // Renamed on the main lane, edited on the lane.
    let mut w = shop();
    let lane = w.open();
    w.on_main(&[("src/shop.ts", None), ("src/store.ts", Some(SHOP))]);
    w.on_lane(lane, &[("src/shop.ts", Some(&edited))]);
    w.integrate(lane);
    assert_eq!(w.main_file("src/store.ts"), edited);
    assert!(w.main.read(&w.blobs, "src/shop.ts").is_none());
    assert_eq!(w.main.id("src/store.ts", "total"), id);

    // Removed on the lane and untouched on the main lane: removed. New
    // files of the lane simply arrive.
    let mut w = shop();
    let lane = w.open();
    w.on_lane(lane, &[("README.md", None), ("src/shop.ts", None), ("docs/notes.md", Some("notes\n"))]);
    w.on_main(&[("src/other.ts", Some("export const other = 1;\n"))]);
    w.integrate(lane);
    assert_eq!(w.main_files().keys().collect::<Vec<_>>(), ["docs/notes.md", "src/other.ts"]);
    assert!(w.main.table.in_file(std::path::Path::new("src/shop.ts")).next().is_none());

    // Removed on the lane, but the main lane changed a function in it.
    let mut w = shop();
    let lane = w.open();
    w.on_main(&[("src/shop.ts", Some(&edited))]);
    w.on_lane(lane, &[("src/shop.ts", None)]);
    assert_eq!(describe(&w.merge(lane)), ["RemoveModify total in src/shop.ts"]);
}

#[test]
fn files_only_the_main_lane_changed_are_not_even_read() {
    // Forty modules. The main lane goes on in thirty of them while the lane
    // edits one function of another: integrating reads that module only.
    let files: Vec<(String, String)> = (0..40).map(|n| (format!("src/m{n}.ts"), module(3))).collect();
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
    let mut w = World::new(&files);
    let lane = w.open();
    for n in 0..30 {
        let edited = module(3).replace("return y * 2;", &format!("return y * {};", n + 3));
        w.on_main(&[(&format!("src/m{n}.ts"), Some(&edited))]);
    }
    let edited = module(3).replace("const y = x + 1;", "const y = x + 10;");
    w.on_lane(lane, &[("src/m35.ts", Some(&edited))]);

    let read = std::cell::RefCell::new(std::collections::BTreeSet::new());
    let parser = |path: &std::path::Path, _: &BlobId, content: &[u8]| {
        read.borrow_mut().insert(path.to_owned());
        kito_parse::parse(kito_parse::dialect_for_path(path)?, content)
    };
    let (state, base) = &w.lanes[lane];
    let outcome = kito_merge::merge(&kito_merge::Input {
        base: kito_merge::Side { tree: &base.tree, symbols: &base.table },
        main: kito_merge::Side { tree: &w.main.tree, symbols: &w.main.table },
        lane: kito_merge::Side { tree: &state.tree, symbols: &state.table },
        blob: &|id| w.blobs.get(id).cloned(),
        resolutions: &Default::default(),
        parse: Some(&parser),
    });
    assert!(outcome.conflicts.is_empty(), "{:?}", describe(&outcome));
    assert_eq!(outcome.files.keys().collect::<Vec<_>>(), [std::path::Path::new("src/m35.ts")]);
    assert_eq!(read.into_inner().into_iter().collect::<Vec<_>>(), [std::path::PathBuf::from("src/m35.ts")]);

    // The same when a symbol crossed files on the main lane: the file it
    // went to is tied to the lane's work, and is read.
    let mut w = shop();
    let lane = w.open();
    let start = SHOP.find("export function discount").unwrap();
    let end = SHOP.find("export class Cart").unwrap();
    let (function, rest) = (&SHOP[start..end], format!("{}{}", &SHOP[..start], &SHOP[end..]));
    w.on_main(&[("src/shop.ts", Some(&rest)), ("src/prices.ts", Some(function))]);
    w.on_main(&[("README.md", Some("# Shop\n\nA demo, edited.\n"))]);
    w.on_lane(
        lane,
        &[("src/shop.ts", Some(&SHOP.replace("return amount - cut;", "return amount - cut - 0;")))],
    );
    w.integrate(lane);
    assert!(w.main_file("src/prices.ts").contains("return amount - cut - 0;"));
    assert!(w.main_file("README.md").contains("edited"));
}

#[test]
fn a_lane_that_did_nothing_changes_nothing() {
    let mut w = shop();
    let lane = w.open();
    w.on_main(&[("src/shop.ts", Some(&SHOP.replace("let sum = 0;", "let sum = 1;")))]);
    let outcome = w.merge(lane);
    assert!(outcome.files.is_empty() && outcome.conflicts.is_empty());
    // And one that did what the main lane also did.
    w.on_lane(lane, &[("src/shop.ts", Some(&SHOP.replace("let sum = 0;", "let sum = 1;")))]);
    let outcome = w.merge(lane);
    assert!(outcome.files.is_empty() && outcome.conflicts.is_empty());
}

/// One file in three versions, merged as two lanes that land one after the
/// other: `first` then `second`. Returns the world and the second merge.
fn three(name: &str, base: &str, first: &str, second: &str) -> (World, usize, kito_merge::Outcome) {
    let mut w = World::new(&[(name, base)]);
    let (a, b) = (w.open(), w.open());
    w.on_lane(a, &[(name, Some(first))]);
    w.on_lane(b, &[(name, Some(second))]);
    w.integrate(a);
    let outcome = w.merge(b);
    (w, b, outcome)
}

#[test]
fn a_line_both_sides_added_where_their_changes_touch_is_there_once() {
    // One side changes a line and adds a line after it; the other adds the
    // same line at that place. (requests 3347146a441d, README.rst)
    let (base, ours, theirs) = ("one\ntwo\nthree\n", "one\nTWO\nadded\nthree\n", "one\ntwo\nadded\nthree\n");
    for (first, second) in [(ours, theirs), (theirs, ours)] {
        let (mut w, lane, outcome) = three("notes.txt", base, first, second);
        assert!(outcome.conflicts.is_empty(), "{:?}", describe(&outcome));
        w.integrate(lane);
        assert_eq!(w.main_file("notes.txt"), "one\nTWO\nadded\nthree\n");
    }
}

const REDIS: &str = "class Redis(object):\n    def get(self, name):\n        return self._get_value()\n\n    def incr(self, name):\n        return self._get_numeric_response()\n";

/// `REDIS` with a method `mget` after `get`, which ends in `last`.
fn with_mget(last: &str) -> String {
    REDIS.replace(
        "        return self._get_value()\n",
        &format!(
            "        return self._get_value()\n\n    def mget(self, *args):\n        self._write('MGET %s\\r\\n' % ' '.join(args))\n        return self.{last}()\n"
        ),
    )
}

#[test]
fn what_both_sides_added_the_same_is_in_the_result_once() {
    // Both sides add the same method at the same place; one of them also
    // changes another method, so the two files are not the same.
    let ours = with_mget("get_response");
    let theirs = ours.replace("_get_numeric_response", "get_response");
    for (first, second) in [(&ours, &theirs), (&theirs, &ours)] {
        let (mut w, lane, outcome) = three("redis.py", REDIS, first, second);
        assert!(outcome.conflicts.is_empty(), "{:?}", describe(&outcome));
        // It is the symbol the first lane made, and the merge says so.
        let kept = w.main.id("redis.py", "Redis.mget");
        assert_eq!((kept.origin(), outcome.added_by_both.as_slice()), (LaneId(1), &[kept][..]));
        w.integrate(lane);
        assert_eq!(w.main_file("redis.py"), theirs);
        assert_eq!(w.main.id("redis.py", "Redis.mget"), kept);
    }

    // A function at the top of a file, where one side changes the one above.
    let base = "def one():\n    return 1\n";
    let ours = "def one():\n    return 1\n\n\ndef two():\n    return 2\n";
    let theirs = "def one():\n    return 11\n\n\ndef two():\n    return 2\n";
    let (mut w, lane, outcome) = three("t.py", base, ours, theirs);
    assert!(outcome.conflicts.is_empty(), "{:?}", describe(&outcome));
    w.integrate(lane);
    assert_eq!(w.main_file("t.py"), theirs);

    // What only looks the same is not: the text that leads to it differs.
    let theirs = "def one():\n    return 11\n\n\n# Twice one.\ndef two():\n    return 2\n";
    let (_, _, outcome) = three("t.py", base, ours, theirs);
    assert_eq!(describe(&outcome), ["Text t.py in t.py"]);
}

#[test]
fn two_things_of_one_name_added_by_both_sides_are_a_conflict() {
    // Both sides add a method `mget` after `get`: the two end differently.
    // (redis 0c9ca0e11ca2, redis.py: `mget` and `info`)
    let (ours, theirs) = (with_mget("get_response"), with_mget("_get_multi_response"));
    let (w, lane, outcome) = three("redis.py", REDIS, &ours, &theirs);
    assert_eq!(describe(&outcome), ["BodyBody Redis.mget in redis.py"]);
    let conflict = &outcome.conflicts[0];
    let mine = w.main.id("redis.py", "Redis.mget");
    assert_eq!(conflict.subject, Subject::Symbol(mine));
    let text = |t: &Option<Vec<u8>>| t.as_ref().map(|t| String::from_utf8(t.clone()).unwrap());
    assert_eq!(conflict.base, None, "nobody had it before");
    assert!(text(&conflict.main).unwrap().ends_with("return self.get_response()\n"));
    assert!(text(&conflict.lane).unwrap().ends_with("return self._get_multi_response()\n"));

    // Each way of deciding it leaves one method.
    let kept = w.merge_with(lane, &[(Subject::Symbol(mine), Choice::Main)]);
    assert!(kept.conflicts.is_empty() && kept.files.is_empty(), "the main lane stays as it is");
    let file = |outcome: &kito_merge::Outcome| {
        let file = outcome.files[std::path::Path::new("redis.py")].clone().unwrap();
        String::from_utf8(file.content).unwrap()
    };
    let taken = w.merge_with(lane, &[(Subject::Symbol(mine), Choice::Lane)]);
    assert_eq!(file(&taken), theirs);
    let written = "    def mget(self, *args):\n        return self.both()";
    let decided = w.merge_with(lane, &[(Subject::Symbol(mine), Choice::Text(written.into()))]);
    assert!(decided.conflicts.is_empty());
    assert_eq!(file(&decided).matches("def mget").count(), 1);
    assert!(file(&decided).contains("return self.both()\n\n    def incr"), "{}", file(&decided));

    // Taking the lane's is taking its symbol.
    let mut w = w;
    let theirs_id = w.lanes[lane].0.id("redis.py", "Redis.mget");
    w.integrate_with(lane, &[(Subject::Symbol(mine), Choice::Lane)]);
    assert_eq!(w.main_file("redis.py"), theirs);
    assert_eq!(w.main.id("redis.py", "Redis.mget"), theirs_id);

    // Where the language lets two things of one name live side by side,
    // they do: two overloads, two blocks of methods.
    let base = "class Log {\n    void write(String s) {\n        out.print(s);\n    }\n}\n";
    let add = |method: &str| base.replace("    }\n}\n", &format!("    }}\n\n{method}}}\n"));
    let ours = add("    void log(int n) {\n        write(String.valueOf(n));\n    }\n");
    let theirs = add("    void log(String s) {\n        write(s);\n    }\n");
    let (mut w, lane, outcome) = three("Log.java", base, &ours, &theirs);
    assert!(outcome.conflicts.is_empty(), "{:?}", describe(&outcome));
    w.integrate(lane);
    let merged = w.main_file("Log.java");
    assert!(merged.contains("void log(int n)") && merged.contains("void log(String s)"), "{merged}");
    // The same parameters are the same overload.
    let theirs = add("    void log(int n) {\n        write(Integer.toString(n));\n    }\n");
    let (_, _, outcome) = three("Log.java", base, &ours, &theirs);
    assert_eq!(describe(&outcome), ["BodyBody Log.log in Log.java"]);

    let base = "struct Shape;\n";
    let ours = "struct Shape;\n\nimpl Shape {\n    fn sides(&self) -> usize {\n        4\n    }\n}\n";
    let theirs = "struct Shape;\n\nimpl Shape {\n    fn new() -> Shape {\n        Shape\n    }\n}\n";
    let (mut w, lane, outcome) = three("shape.rs", base, ours, theirs);
    assert!(outcome.conflicts.is_empty(), "{:?}", describe(&outcome));
    w.integrate(lane);
    assert_eq!(w.main_file("shape.rs").matches("impl Shape {").count(), 2);
}

#[test]
fn a_text_conflict_that_a_line_merge_does_not_have_is_settled_by_lines() {
    // One side adds a function after the statements that follow the last
    // function of the file; the other changes one of those statements.
    // (express f9256ef36fa9, test/app.router.js)
    let cases = [
        (
            "t.py",
            "def helper():\n    return 0\n\n\nstart('x')\nstart('z')\n",
            "\n\ndef added():\n    return 1\n",
            ("start('x')", "start('y')"),
        ),
        (
            "t.js",
            "function helper() {\n  return 0;\n}\n\nstart('x');\nstart('z');\n",
            "\nfunction added() {\n  return 1;\n}\n",
            ("start('x')", "start('y')"),
        ),
        (
            "t.c",
            "int helper(void) {\n    return 0;\n}\n\nint x = 1;\nint z = 3;\n",
            "\nint added(void) {\n    return 1;\n}\n",
            ("int x = 1;", "int x = 2;"),
        ),
    ];
    for (name, base, function, (from, to)) in cases {
        let (adds, changes) = (format!("{base}{function}"), base.replace(from, to));
        let both = format!("{}{function}", base.replace(from, to));
        for (first, second) in [(&adds, &changes), (&changes, &adds)] {
            let (mut w, lane, outcome) = three(name, base, first, second);
            assert!(outcome.conflicts.is_empty(), "{name}: {:?}", describe(&outcome));
            // The merge says which file it merged by lines.
            assert_eq!(outcome.merged_by_lines, [std::path::PathBuf::from(name)], "{name}");
            w.integrate(lane);
            assert_eq!(w.main_file(name), both, "{name}");
            // The new function is the symbol its lane made.
            let adder = if first == &adds { LaneId(1) } else { LaneId(2) };
            assert_eq!(w.main.id(name, "added").origin(), adder, "{name}");
        }
    }

    // Where a line merge stops too, the conflict stands: both sides change
    // the same statement.
    let base = "def helper():\n    return 0\n\n\nstart('x')\nstart('z')\n";
    let ours = format!("{}\n\ndef added():\n    return 1\n", base.replace("start('x')", "start('w')"));
    let (_, _, outcome) = three("t.py", base, &ours, &base.replace("start('x')", "start('y')"));
    assert_eq!(describe(&outcome), ["Text t.py in t.py"]);
    assert!(outcome.merged_by_lines.is_empty());
}

#[test]
fn a_line_merge_that_would_keep_a_function_twice_is_not_taken() {
    // Both sides add the same function, one at the top of the file and one
    // after the statements that close it, and one of them also changes a
    // statement. The merge by symbols knows the two are one function, and
    // stops on the text around them; the lines compose, and keep both.
    let base = "def helper():\n    return 0\n\n\nstart('x')\nstart('z')\n";
    let added = "def added():\n    return 1\n";
    let at_the_end = format!("{base}\n\n{added}");
    let at_the_top = format!("{added}\n\n{}", base.replace("start('x')", "start('y')"));
    for (first, second) in [(&at_the_end, &at_the_top), (&at_the_top, &at_the_end)] {
        let (_, _, outcome) = three("t.py", base, first, second);
        assert_eq!(describe(&outcome), ["Text t.py in t.py"]);
        assert!(outcome.merged_by_lines.is_empty());
    }
    // The lines alone do compose, with the function twice: that is the
    // result that was not taken.
    let lines = kito_diff::text::merge3_apart(base.as_bytes(), at_the_end.as_bytes(), at_the_top.as_bytes());
    assert_eq!(String::from_utf8(lines.unwrap()).unwrap().matches("def added").count(), 2);

    // One side renames a function and moves it, the other moves it
    // elsewhere: the lines keep it under its two names, and each is known
    // to a side. A symbol is one definition: that result is not taken either.
    let foo = "def foo(x):\n    y = x + 1\n    return y * 2\n\n\n";
    let rest = "def f1(x):\n    a = x\n    b = a\n    c = b\n    d = c\n    return d\n\n\ndef f2(x):\n    return x - 2\n\n\ndef f3(x):\n    return x - 3\n\n\ndef f4(x):\n    return x - 4\n\n\nstart('x')\nstart('z')\n";
    let base = rest.replace("def f2", &format!("{foo}def f2"));
    let renamed = format!(
        "{}\n\ndef added():\n    return 1\n",
        rest.replace("def f4", &format!("{}def f4", foo.replace("def foo", "def bar")))
    );
    let moved = format!("{foo}{}", rest.replace("start('x')", "start('y')"));
    let lines = kito_diff::text::merge3_apart(base.as_bytes(), renamed.as_bytes(), moved.as_bytes());
    let lines = String::from_utf8(lines.unwrap()).unwrap();
    assert!(lines.contains("def foo(x)") && lines.contains("def bar(x)"), "{lines}");
    for (first, second) in [(&renamed, &moved), (&moved, &renamed)] {
        let (_, _, outcome) = three("t.py", &base, first, second);
        assert!(!outcome.conflicts.is_empty(), "the file is asked about");
        assert!(
            outcome.conflicts.iter().all(|c| c.kind == kito_core::ConflictKind::Text),
            "{:?}",
            describe(&outcome)
        );
        assert!(outcome.merged_by_lines.is_empty());
    }
}

#[test]
fn a_symbol_a_side_moved_into_a_scope_is_not_one_the_other_added_there() {
    // One side changes a method and copies it, as it was, into another
    // class; the other side moves the method to that class. The class has
    // the same text on both sides, made of two different methods: the
    // moved one is the method itself, and what was changed in it where it
    // stood must not be lost to the copy. The two cannot both stay under
    // one name, and are not one thing added twice: the file is asked
    // about, whichever side came first.
    let helper = "    def helper(self, items):\n        total = 0\n        for item in items:\n            total += item.size\n        return total\n";
    let base = format!(
        "class A:\n{helper}\n    def other(self):\n        return 2\n\n\nclass B:\n    def run(self):\n        return 3\n"
    );
    let changed = base.replace(
        "            total += item.size\n",
        "            if item is not None:\n                total += item.size\n",
    );
    let copies = format!("{changed}\n{helper}");
    let moves = format!("{}\n{helper}", base.replace(&format!("{helper}\n"), ""));
    for (first, second) in [(&copies, &moves), (&moves, &copies)] {
        let (_, _, outcome) = three("t.py", &base, first, second);
        assert!(outcome.added_by_both.is_empty(), "the two are not one thing added twice");
        assert_eq!(describe(&outcome), ["Text t.py in t.py"]);
        assert!(outcome.files.is_empty() && outcome.merged_by_lines.is_empty());
    }

    // The same when the class is not the same text on the two sides: the
    // side that moves the method also adds one after it.
    let more = format!("{moves}\n    def last(self):\n        return 4\n");
    for (first, second) in [(&copies, &more), (&more, &copies)] {
        let (_, _, outcome) = three("t.py", &base, first, second);
        assert!(outcome.added_by_both.is_empty(), "the two are not one thing added twice");
        assert_eq!(describe(&outcome), ["Text t.py in t.py"]);
    }

    // And across files: the function goes to another file on one side,
    // where the other side wrote a copy of it. That file is the same on
    // both sides, byte for byte, and is still two different things.
    let helper = "def helper(items):\n    total = 0\n    for item in items:\n        total += item.size\n    return total\n";
    let a = format!("{helper}\n\ndef other():\n    return 2\n");
    let b = "def run():\n    return 3\n";
    let a_changed = a.replace(
        "        total += item.size\n",
        "        if item is not None:\n            total += item.size\n",
    );
    let b_with_it = format!("{b}\n\n{helper}");
    for copies_first in [true, false] {
        let mut w = World::new(&[("a.py", &a), ("b.py", b)]);
        let (copies, moves) = (w.open(), w.open());
        w.on_lane(copies, &[("a.py", Some(&a_changed)), ("b.py", Some(&b_with_it))]);
        w.on_lane(moves, &[("a.py", Some("def other():\n    return 2\n")), ("b.py", Some(&b_with_it))]);
        assert_eq!(
            w.lanes[moves].0.id("b.py", "helper"),
            w.main.id("a.py", "helper"),
            "it moved, and is itself"
        );
        let (first, second) = if copies_first { (copies, moves) } else { (moves, copies) };
        w.integrate(first);
        let outcome = w.merge(second);
        let merged: Vec<String> = outcome
            .files
            .iter()
            .map(|(path, file)| {
                let text = file.as_ref().map(|f| String::from_utf8_lossy(&f.content).into_owned());
                format!("{}: {text:?}", path.display())
            })
            .collect();
        assert_eq!(describe(&outcome), ["Text b.py in b.py"], "copies first: {copies_first}; {merged:#?}");
    }
}

#[test]
fn a_setter_one_side_changed_is_not_the_getter_of_the_other() {
    // One side adds a getter above a setter, the other changes the setter:
    // once in each language where the two are defined under one name.
    let cases = [
        (
            "cart.js",
            "class Cart {\n  set total(value) {\n    this.cents = Math.round(value * 100);\n  }\n}\n",
            "  get total() {\n    return this.cents / 100;\n  }\n\n",
            "Cart.set total",
        ),
        (
            "cart.ts",
            "class Cart {\n  set total(value: number) {\n    this.cents = Math.round(value * 100);\n  }\n}\n",
            "  get total(): number {\n    return this.cents / 100;\n  }\n\n",
            "Cart.set total",
        ),
    ];
    for (name, base, getter, setter) in cases {
        let ours = base.replacen("  set total", &format!("{getter}  set total"), 1);
        let theirs = base.replace("Math.round", "Math.floor");
        let (mut w, lane, outcome) = three(name, base, &ours, &theirs);
        assert!(outcome.conflicts.is_empty(), "{name}: {:?}", describe(&outcome));
        let id = w.main.id(name, setter);
        w.integrate(lane);
        assert_eq!(w.main_file(name), ours.replace("Math.round", "Math.floor"), "{name}");
        assert_eq!(w.main.id(name, setter), id, "{name}");
    }

    // Python: a property goes to another class of another file, where its
    // getter says more; the other side changes its setter where it was.
    // (flask 864875099762, helpers.py and scaffold.py)
    let helpers = "import os\n\n\nclass Box(object):\n    def other(self):\n        return 1\n\n    @property\n    def folder(self):\n        \"\"\"The folder.\"\"\"\n        if self._folder is not None:\n            return os.path.join(self.root, self._folder)\n\n    @folder.setter\n    def folder(self, value):\n        if value is not None:\n            value = os.fspath(value).rstrip(\"/\")\n        self._folder = value\n";
    let left = "import os\n\n\nclass Box(object):\n    def other(self):\n        return 1\n";
    let scaffold = "import os\n\n\nclass Scaffold:\n    @property\n    def folder(self):\n        \"\"\"The absolute path of the folder, or ``None``.\"\"\"\n        if self._folder is not None:\n            return os.path.join(self.root, self._folder)\n        else:\n            return None\n\n    @folder.setter\n    def folder(self, value):\n        if value is not None:\n            value = os.fspath(value).rstrip(\"/\")\n\n        self._folder = value\n";
    let mut w = World::new(&[("helpers.py", helpers)]);
    let setter = w.main.id("helpers.py", "Box.setter folder");
    let (moves, edits) = (w.open(), w.open());
    w.on_lane(moves, &[("helpers.py", Some(left)), ("scaffold.py", Some(scaffold))]);
    w.on_lane(edits, &[("helpers.py", Some(&helpers.replace("os.fspath(value)", "fspath(value)")))]);
    w.integrate(moves);
    assert_eq!(w.main.id("scaffold.py", "Scaffold.setter folder"), setter, "the setter moved, and is itself");
    let outcome = w.integrate(edits);
    assert_eq!(outcome.merged_symbols, [setter]);
    assert_eq!(w.main_file("helpers.py"), left);
    assert_eq!(w.main_file("scaffold.py"), scaffold.replace("os.fspath(value)", "fspath(value)"));
}

#[test]
fn namesakes_that_are_not_the_ones_of_the_base_are_not_merged_as_symbols() {
    // One side writes a new overload above an old one, the other changes a
    // line of the old one that the new one has too. The history numbers
    // namesakes in the order they were made, the file in the order they are
    // written: the change must not land in the new overload.
    let cases = [
        (
            "Log.java",
            "class Log {\n    void log(int n) {\n        write(prefix);\n        write(String.valueOf(n));\n    }\n}\n",
            "    void log(int n) {",
            "    void log(String s) {\n        write(prefix);\n        write(s);\n    }\n\n",
        ),
        (
            "Log.cs",
            "class Log {\n    void Write(int n) {\n        Put(prefix);\n        Put(n.ToString());\n    }\n}\n",
            "    void Write(int n) {",
            "    void Write(string s) {\n        Put(prefix);\n        Put(s);\n    }\n\n",
        ),
        (
            "log.cpp",
            "void log(int n) {\n    write(prefix);\n    write(std::to_string(n));\n}\n",
            "void log(int n) {",
            "void log(const std::string& s) {\n    write(prefix);\n    write(s);\n}\n\n",
        ),
        (
            "shape.js",
            "class Shape {\n  from(other) {\n    this.prefix = other.prefix;\n    return this;\n  }\n}\n",
            "  from(other) {",
            "  static from(other) {\n    this.prefix = other.prefix;\n    return new Shape();\n  }\n\n",
        ),
    ];
    for (name, base, old, new) in cases {
        let ours = base.replacen(old, &format!("{new}{old}"), 1);
        let theirs = base.replace("prefix);", "prefix + 1);").replace("other.prefix;", "other.prefix + 1;");
        assert_ne!(theirs, base, "{name}");
        let both = theirs.replacen(old, &format!("{new}{old}"), 1);
        for (first, second) in [(&ours, &theirs), (&theirs, &ours)] {
            let (mut w, lane, outcome) = three(name, base, first, second);
            assert!(outcome.conflicts.is_empty(), "{name}: {:?}", describe(&outcome));
            w.integrate(lane);
            assert_eq!(w.main_file(name), both, "{name}");
        }
    }
}

#[test]
fn a_file_with_a_symbol_from_another_file_is_not_settled_by_its_lines_alone() {
    // The main lane moves a function into a file and changes a statement
    // at its end; the lane changes that function where it was, and adds a
    // function after the statements. The lines of the file compose, and
    // would leave the lane's change to the moved function behind: the
    // conflict stands.
    let a = "def moved():\n    return 1\n\n\ndef stays():\n    return 2\n";
    let b = "def helper():\n    return 0\n\n\nstart('x')\nstart('z')\n";
    let mut w = World::new(&[("a.py", a), ("b.py", b)]);
    let lane = w.open();
    w.on_main(&[
        ("a.py", Some("def stays():\n    return 2\n")),
        ("b.py", Some(&format!("def moved():\n    return 1\n\n\n{}", b.replace("start('x')", "start('y')")))),
    ]);
    w.on_lane(
        lane,
        &[
            ("a.py", Some(&a.replace("return 1", "return 11"))),
            ("b.py", Some(&format!("{b}\n\ndef added():\n    return 1\n"))),
        ],
    );
    assert_eq!(w.main.id("b.py", "moved"), w.lanes[lane].0.id("a.py", "moved"), "it moved, and is itself");
    let outcome = w.merge(lane);
    assert_eq!(describe(&outcome), ["Text b.py in b.py"]);
    assert!(outcome.merged_by_lines.is_empty());

    // Namesakes that cannot be told apart, in a file a method came to from
    // another file: neither its symbols nor its lines can be trusted, and
    // the file is asked about.
    let log = "class Log {\n    void log(int n) {\n        write(prefix);\n        write(String.valueOf(n));\n    }\n}\n";
    let flush =
        "    void flush() {\n        out.flush();\n        count = count + 1;\n        done = true;\n    }\n";
    let util = format!("class Util {{\n{flush}\n    void other() {{\n        out.close();\n    }}\n}}\n");
    let mut w = World::new(&[("Log.java", log), ("Util.java", &util)]);
    let lane = w.open();
    let overload = "    void log(String s) {\n        write(prefix);\n        write(s);\n    }\n\n";
    let grown = log
        .replacen("    void log(int n) {", &format!("{overload}    void log(int n) {{"), 1)
        .replace("    }\n}\n", &format!("    }}\n\n{flush}}}\n"));
    w.on_main(&[("Log.java", Some(&grown)), ("Util.java", Some(&util.replace(&format!("{flush}\n"), "")))]);
    w.on_lane(lane, &[("Log.java", Some(&log.replace("prefix);", "prefix + 1);")))]);
    assert_eq!(describe(&w.merge(lane)), ["Text Log.java in Log.java"]);
}

/// A module of `n` functions, each with a body that tells them apart.
fn module(n: usize) -> String {
    (0..n)
        .map(|i| format!("export function f{i}(x: number): number {{\n  const y = x + {i};\n  log(\"f{i}\", y);\n  return y * 2;\n}}\n"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Debug)]
enum Edit {
    /// Changes the body of the function.
    Body,
    /// Renames it.
    Rename,
    /// Adds a new function right after it.
    AddAfter,
    /// Removes it.
    Remove,
}

fn apply_edit(text: &str, function: usize, edit: &Edit, lane: usize) -> String {
    let start = text.find(&format!("export function f{function}(")).expect("the function is there");
    let end = start + text[start..].find("}\n").unwrap() + 2;
    let body = &text[start..end];
    let new = match edit {
        Edit::Body => body.replace("return y * 2;", &format!("return y * {};", lane + 3)),
        Edit::Rename => body.replace(&format!("f{function}("), &format!("g{function}_{lane}(")),
        Edit::AddAfter => format!(
            "{body}\nexport function added{function}_{lane}(): string {{\n  return \"lane {lane} after f{function}\";\n}}\n"
        ),
        Edit::Remove => String::new(),
    };
    let mut out = format!("{}{}{}", &text[..start], new, &text[end..]);
    if matches!(edit, Edit::Remove) {
        // Not two blank lines where the function was, nor one at the end.
        out = out.replace("\n\n\n", "\n\n");
        while out.ends_with("\n\n") {
            out.pop();
        }
        if let Some(rest) = out.strip_prefix('\n') {
            out = rest.to_owned();
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Lanes that work on different functions of the same file never
    /// conflict, and the result does not depend on who integrates first.
    #[test]
    fn work_on_different_symbols_commutes(
        plan in prop::collection::vec(
            prop::sample::select(vec![Edit::Body, Edit::Rename, Edit::AddAfter, Edit::Remove]),
            6,
        ),
        owners in prop::collection::vec(0usize..3, 6),
        order in Just(vec![0usize, 1, 2]).prop_shuffle(),
    ) {
        let base = module(6);
        let build = |order: &[usize]| {
            let mut w = World::new(&[("m.ts", &base)]);
            let lanes: Vec<usize> = (0..3).map(|_| w.open()).collect();
            for &lane in &lanes {
                let mut text = base.clone();
                for (function, edit) in plan.iter().enumerate() {
                    if owners[function] == lane {
                        text = apply_edit(&text, function, edit, lane);
                    }
                }
                if text != base {
                    let content = (!text.is_empty()).then_some(text.as_str());
                    w.on_lane(lane, &[("m.ts", content)]);
                }
            }
            for &lane in order {
                let outcome = w.merge(lane);
                prop_assert!(outcome.conflicts.is_empty(), "fake conflict: {:?}", describe(&outcome));
                w.integrate(lane);
            }
            Ok(w)
        };
        let first = build(&[0, 1, 2])?;
        let second = build(&order)?;
        prop_assert_eq!(first.main_files(), second.main_files());

        // Every edit of every lane is in the result.
        let merged = first.main.read(&first.blobs, "m.ts").unwrap_or_default();
        for (function, edit) in plan.iter().enumerate() {
            let lane = owners[function];
            let expected = match edit {
                Edit::Body => format!("log(\"f{function}\", y);\n  return y * {};", lane + 3),
                Edit::Rename => format!("export function g{function}_{lane}("),
                Edit::AddAfter => format!("export function added{function}_{lane}()"),
                Edit::Remove => String::new(),
            };
            prop_assert!(merged.contains(&expected), "{expected} is missing from:\n{merged}");
            if matches!(edit, Edit::Remove) {
                let gone = format!("function f{function}(");
                prop_assert!(!merged.contains(&gone), "{gone} is still in:\n{merged}");
            }
        }
        // Symbols keep the ID they had, or the one their lane gave them.
        for (function, edit) in plan.iter().enumerate() {
            let lane = owners[function];
            match edit {
                Edit::Body => prop_assert_eq!(first.main.id("m.ts", &format!("f{function}")).origin(), LaneId::MAIN),
                Edit::Rename => {
                    let id = first.main.id("m.ts", &format!("g{function}_{lane}"));
                    prop_assert_eq!(id, SymbolId::new(LaneId::MAIN, function as u64));
                }
                Edit::AddAfter => {
                    let id = first.main.id("m.ts", &format!("added{function}_{lane}"));
                    prop_assert_eq!(id.origin(), LaneId(lane as u64 + 1));
                }
                Edit::Remove => {}
            }
        }
    }
}
