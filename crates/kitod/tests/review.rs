//! Review of a lane (ADR 0004): the diff of a lane.

use kito_core::paths::Canonical as _;
use kito_core::*;
use kitod::review::{Change, FileDiff, LaneDiff};
use kitod::Repo;
use std::fs;
use std::path::{Path, PathBuf};

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn isolate_identity() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let empty = std::env::temp_dir().join(format!("kito-test-config-{}", std::process::id()));
        std::env::set_var("KITO_CONFIG_DIR", &empty);
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("KITO_AUTHOR", "ada");
        std::env::remove_var("KITO_EMAIL");
    });
}

const SHOP: &str = r#"export function total(prices: number[]): number {
  let sum = 0;
  for (const price of prices) {
    sum += price;
  }
  return sum;
}

export function discount(amount: number, percent: number): number {
  const cut = Math.round((amount * percent) / 100);
  return amount - cut;
}
"#;

const CART: &str = r#"import { total } from "./shop";

export function checkout(prices: number[]): string {
  return `to pay: ${total(prices)}`;
}
"#;

fn shop() -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    drop(Repo::init(&root).unwrap());
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    write(&root, "src/cart.ts", CART);
    write(&root, "README.md", "# Shop\n");
    write(&root, "logo.bin", "\u{0}\u{1}\u{2}logo");
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn open(repo: &mut Repo, agent: &str, intent: &str) -> (LaneId, PathBuf) {
    let info = repo.open_lane(agent, None, None, intent, &["./".to_owned()]).unwrap();
    (info.id.parse().unwrap(), info.workdir.unwrap())
}

fn file<'a>(diff: &'a LaneDiff, path: &str) -> &'a FileDiff {
    diff.files.iter().find(|file| file.path == path).unwrap_or_else(|| panic!("no {path} in {diff:#?}"))
}

/// The symbols of a file's diff, a line each.
fn symbols(file: &FileDiff) -> Vec<String> {
    file.symbols
        .iter()
        .map(|symbol| {
            let was = symbol.was.as_ref().map(|was| format!(" (was {was})")).unwrap_or_default();
            let lines = symbol.lines.map(|(from, to)| format!(" {from}-{to}")).unwrap_or_default();
            format!("{} {}{was}{lines} {:?}", symbol.change.word(), symbol.symbol, symbol.intents)
        })
        .collect()
}

#[test]
fn a_lane_that_changed_nothing_has_an_empty_diff_and_the_main_lane_has_none() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, _work) = open(&mut repo, "alice", "prices that are never negative");
    let diff = repo.lane_diff(lane, &[]).unwrap();
    assert_eq!((diff.lane.as_str(), diff.onto.as_str(), diff.base.as_str()), ("lane/1", "main", "ckpt#1"));
    assert!(diff.files.is_empty() && diff.behind == 0, "{diff:#?}");
    let said = repo.lane_diff(LaneId::MAIN, &[]).unwrap_err().to_string();
    assert!(said.contains("the main lane has no diff"), "{said}");
}

#[test]
fn the_diff_of_a_lane_is_its_files_its_functions_and_its_lines() {
    let (_tmp, root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", "prices that are never negative");

    // A body changed, a function added, a file added, one removed, a
    // binary one replaced.
    let edited = SHOP.replace("sum += price;", "sum += Math.max(price, 0);")
        + "\nexport function tax(amount: number): number {\n  return Math.round(amount * 0.22);\n}\n";
    write(&work, "src/shop.ts", &edited);
    write(&work, "src/tax.md", "# Tax\n\n22%.\n");
    fs::remove_file(work.join("README.md")).unwrap();
    write(&work, "logo.bin", "\u{0}\u{1}\u{2}new logo");
    repo.scan_lane(lane).unwrap();

    let diff = repo.lane_diff(lane, &[]).unwrap();
    let paths: Vec<(&str, Change)> =
        diff.files.iter().map(|file| (file.path.as_str(), file.change)).collect();
    assert_eq!(
        paths,
        [
            ("README.md", Change::Removed),
            ("logo.bin", Change::Changed),
            ("src/shop.ts", Change::Changed),
            ("src/tax.md", Change::Added)
        ]
    );

    let shop = file(&diff, "src/shop.ts");
    assert_eq!((shop.added, shop.removed, shop.binary), (5, 1, false));
    assert_eq!(
        symbols(shop),
        [
            "changed src/shop.ts:total 1-7 [\"prices that are never negative\"]",
            "added src/shop.ts:tax 14-16 [\"prices that are never negative\"]",
        ]
    );
    assert_eq!(shop.hunks.len(), 2, "{:#?}", shop.hunks);
    assert_eq!((shop.hunks[0].before, shop.hunks[0].after), ((1, 7), (1, 7)));
    assert_eq!(
        shop.hunks[0].lines[2..5],
        ["   for (const price of prices) {", "-    sum += price;", "+    sum += Math.max(price, 0);"]
    );
    // The contents are there to be read whole.
    let before = String::from_utf8(repo.content(shop.before.as_ref().unwrap()).unwrap()).unwrap();
    let after = String::from_utf8(repo.content(shop.after.as_ref().unwrap()).unwrap()).unwrap();
    assert_eq!((before.as_str(), after.as_str()), (SHOP, edited.as_str()));

    let readme = file(&diff, "README.md");
    assert_eq!((readme.added, readme.removed, readme.after.is_none()), (0, 1, true));
    assert_eq!(readme.hunks[0].lines, ["-# Shop"]);
    let tax = file(&diff, "src/tax.md");
    assert_eq!((tax.added, tax.removed, tax.before.is_none()), (3, 0, true));
    let logo = file(&diff, "logo.bin");
    assert!(logo.binary && logo.hunks.is_empty() && (logo.added, logo.removed) == (0, 0));

    // Asked about some paths: a file, a directory.
    let some = repo.lane_diff(lane, &["src/".to_owned()]).unwrap();
    assert_eq!(
        some.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
        ["src/shop.ts", "src/tax.md"]
    );
    let one = repo.lane_diff(lane, &["README.md".to_owned(), "nothing.ts".to_owned()]).unwrap();
    assert_eq!(one.files.len(), 1);

    // What the main lane got meanwhile is not the lane's doing: it is
    // said apart.
    write(&root, "README.md", "# Shop\n\nOpen every day.\n");
    repo.scan().unwrap();
    let diff = repo.lane_diff(lane, &[]).unwrap();
    assert_eq!((diff.behind, diff.files.len()), (1, 4));
}

#[test]
fn a_rename_is_one_change_with_its_two_names() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", "what the shop takes off is a rebate");

    // A function renamed, with its uses; a file renamed, with what is in it.
    repo.rename_symbol_with(lane, "src/shop.ts:discount", "rebate", None).unwrap();
    repo.set_intent(lane, "the cart is a basket here", None, None).unwrap();
    fs::rename(work.join("src/cart.ts"), work.join("src/basket.ts")).unwrap();
    repo.scan_lane(lane).unwrap();

    let diff = repo.lane_diff(lane, &[]).unwrap();
    let paths: Vec<(&str, Option<&str>, Change)> =
        diff.files.iter().map(|file| (file.path.as_str(), file.was.as_deref(), file.change)).collect();
    assert_eq!(
        paths,
        [("src/basket.ts", Some("src/cart.ts"), Change::Renamed), ("src/shop.ts", None, Change::Changed)]
    );
    assert_eq!(
        symbols(file(&diff, "src/shop.ts")),
        ["renamed src/shop.ts:rebate (was src/shop.ts:discount) 9-12 [\"what the shop takes off is a rebate\"]"]
    );
    // The file kept its content: no lines, and its function is not a
    // change of its own.
    let basket = file(&diff, "src/basket.ts");
    assert_eq!((basket.added, basket.removed, basket.hunks.len()), (0, 0, 0));
    assert_eq!(basket.before, basket.after);
    assert_eq!(symbols(basket), [] as [String; 0]);
    // Asked by the name it had.
    assert_eq!(repo.lane_diff(lane, &["src/cart.ts".to_owned()]).unwrap().files.len(), 1);
}

#[test]
fn a_function_moved_to_another_file_is_one_change() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", "discounts have a file of their own");
    let (kept, moved) = SHOP.split_at(SHOP.find("export function discount").unwrap());
    write(&work, "src/shop.ts", kept);
    write(&work, "src/discount.ts", moved);
    repo.scan_lane(lane).unwrap();

    let diff = repo.lane_diff(lane, &[]).unwrap();
    assert_eq!(
        symbols(file(&diff, "src/discount.ts")),
        ["renamed src/discount.ts:discount (was src/shop.ts:discount) 1-4 [\"discounts have a file of their own\"]"]
    );
    assert_eq!(symbols(file(&diff, "src/shop.ts")), [] as [String; 0]);
}
