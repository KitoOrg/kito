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
