//! The scope of a lane: declared before the first edit, checked against
//! what the lane changes, and compared with what the other lanes are on.

use kito_core::paths::Canonical as _;
use kito_core::*;
use kito_integrate::Outcome;
use kitod::scope::Overlap;
use kitod::Repo;
use std::fs;
use std::path::{Path, PathBuf};

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
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

const SHOP: &str = r#"import { Item } from "./types";

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
"#;

const BILLING: &str = r#"import { total } from "./shop";
import { Item } from "./types";

export function invoice(items: Item[]): string {
  return "to pay: " + total(items);
}

export function footer(): string {
  return "thank you";
}
"#;

const TYPES: &str = "export interface Item {\n  name: string;\n  price: number;\n  quantity: number;\n}\n";

/// A small shop with one checkpoint. `config` is added to the configuration.
fn shop_with(config: &str) -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    drop(Repo::init(&root).unwrap());
    if !config.is_empty() {
        fs::write(root.join(".kito/config.toml"), config).unwrap();
    }
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    write(&root, "src/billing.ts", BILLING);
    write(&root, "src/types.ts", TYPES);
    write(&root, "package-lock.json", "{}\n");
    write(&root, "README.md", "# Shop\n");
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn shop() -> (tempfile::TempDir, PathBuf, Repo) {
    shop_with("")
}

fn scope(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Opens a lane and returns its ID and its working copy.
fn open(repo: &mut Repo, agent: &str, intent: &str, items: &[&str]) -> (LaneId, PathBuf) {
    let info = repo.open_lane(agent, None, None, intent, &scope(items)).unwrap();
    (info.id.parse().unwrap(), info.workdir.unwrap())
}

fn outside(repo: &mut Repo, lane: LaneId) -> Vec<String> {
    repo.scan_lane(lane).unwrap();
    repo.out_of_scope(lane).unwrap()
}

fn told(overlaps: &[Overlap]) -> Vec<(String, String, String)> {
    overlaps.iter().map(|o| (o.kind.clone(), o.lane.clone(), o.what.clone())).collect()
}

fn one(kind: &str, lane: &str, what: &str) -> (String, String, String) {
    (kind.to_owned(), lane.to_owned(), what.to_owned())
}

#[test]
fn no_lane_opens_without_an_intent_and_a_scope() {
    let (_tmp, _root, mut repo) = shop();
    let all = scope(&["./"]);
    let error = |r: kitod::error::Result<kitod::lanes::LaneInfo>| r.unwrap_err().to_string();
    assert!(error(repo.open_lane("alice", None, None, "  ", &all)).contains("needs an intent"));
    assert!(error(repo.open_lane("alice", None, None, "why", &[])).contains("needs a scope"));
    assert!(error(repo.open_lane("alice", None, None, "why", &scope(&[" "]))).contains("needs a scope"));
    assert!(
        error(repo.open_lane("alice", None, None, "why", &scope(&["../x"]))).contains("not a path inside")
    );
    assert!(repo.lanes(false).is_empty(), "a refused lane leaves nothing behind");

    // The scope is on the lane, with the kind of each item, and the intent
    // is the reason of what the lane records from the start.
    let (lane, work) =
        open(&mut repo, "alice", "prices in cents", &["src/shop.ts:total", "src/new/", "README.md"]);
    let info = repo.lane_info(lane).unwrap();
    assert_eq!(info.scope, ["src/shop.ts:total", "src/new/", "README.md"]);
    assert_eq!(info.intent.as_deref(), Some("prices in cents"));
    let kinds: Vec<String> = repo.lane_scope(lane).unwrap().into_iter().map(|d| d.kind).collect();
    assert_eq!(kinds, ["function", "directory", "file"]);
    write(&work, "README.md", "# Shop, in cents\n");
    repo.scan_lane(lane).unwrap();
    let op = &repo.store().lane_ops(lane, 0, 10).unwrap()[0];
    assert_eq!(repo.store().intent(&op.intent.unwrap()).unwrap().summary, "prices in cents");

    // It survives a restart, and only grows.
    let root = repo.root().to_owned();
    repo.close().unwrap();
    let mut repo = Repo::open(&root).unwrap();
    assert_eq!(repo.lane_info(lane).unwrap().scope.len(), 3);
    let grown = repo.extend_scope(lane, &scope(&["src/shop.ts:total", "src/billing.ts"])).unwrap();
    assert_eq!(
        grown.iter().map(|d| d.item.as_str()).collect::<Vec<_>>(),
        ["src/shop.ts:total", "src/new/", "README.md", "src/billing.ts"]
    );
    assert!(repo.extend_scope(lane, &[]).is_err());
    assert!(repo.extend_scope(LaneId::MAIN, &scope(&["./"])).is_err());
}

#[test]
fn a_change_outside_the_scope_is_recorded_and_stops_the_integration() {
    let (_tmp, root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", "prices in cents", &["src/shop.ts:total"]);

    // Inside the function declared: in scope. An import of the same file
    // follows the function that needs it.
    let cents = SHOP.replace("item.price * item.quantity", "(item.price * item.quantity) / 100").replace(
        "import { Item } from \"./types\";",
        "import { Item } from \"./types\";\nimport { round } from \"./math\";",
    );
    write(&work, "src/shop.ts", &cents);
    assert!(outside(&mut repo, lane).is_empty());

    // Another function of the file, another file, a new file: outside.
    write(&work, "src/shop.ts", &cents.replace("Math.round(", "Math.floor("));
    write(&work, "README.md", "# Shop, in cents\n");
    write(&work, "src/extra.ts", "export function extra(): number {\n  return 1;\n}\n");
    assert_eq!(outside(&mut repo, lane), ["README.md", "src/extra.ts", "src/shop.ts:discount"]);
    assert_eq!(repo.lane_info(lane).unwrap().out_of_scope.len(), 3);

    // Nothing is lost, and nothing is merged.
    let refused = repo.integrate(lane).unwrap();
    assert_eq!(refused.result, Outcome::OutOfScope, "{refused:?}");
    assert_eq!(refused.out_of_scope, ["README.md", "src/extra.ts", "src/shop.ts:discount"]);
    assert!(refused.message.unwrap().contains("extend the scope"));
    assert_eq!(read(&root, "src/shop.ts"), SHOP);
    assert_eq!(read(&work, "README.md"), "# Shop, in cents\n");

    // Undo one, declare the others: the lane goes in.
    write(&work, "README.md", "# Shop\n");
    repo.extend_scope(lane, &scope(&["src/shop.ts:discount", "src/extra.ts"])).unwrap();
    assert!(outside(&mut repo, lane).is_empty());
    let done = repo.integrate(lane).unwrap();
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    assert!(
        read(&root, "src/shop.ts").contains("/ 100") && read(&root, "src/shop.ts").contains("Math.floor(")
    );
    assert!(root.join("src/extra.ts").is_file());
    // From the new state on, nothing of the lane differs: nothing is outside.
    assert!(outside(&mut repo, lane).is_empty());
}

#[test]
fn text_outside_the_functions_needs_a_function_of_the_file_or_the_file() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", "a note", &["src/shop.ts:total"]);
    // The top of a file none of whose functions was declared.
    write(&work, "src/billing.ts", &format!("// billing\n{BILLING}"));
    assert_eq!(outside(&mut repo, lane), ["src/billing.ts (outside its functions)"]);
    repo.extend_scope(lane, &scope(&["src/billing.ts:footer"])).unwrap();
    assert!(outside(&mut repo, lane).is_empty());
}

#[test]
fn a_rename_and_what_follows_from_it_are_not_outside() {
    let (_tmp, root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", "a clearer name", &["src/shop.ts:total"]);
    // The function declared gets a new name, and so do its uses in a file
    // the lane did not declare: the import and the call.
    write(&work, "src/shop.ts", &SHOP.replace("function total(", "function sum("));
    write(&work, "src/billing.ts", &BILLING.replace("total", "sum"));
    assert!(outside(&mut repo, lane).is_empty(), "{:?}", repo.out_of_scope(lane));
    // A change of its own in that file is another matter.
    write(&work, "src/billing.ts", &BILLING.replace("total", "sum").replace("thank you", "thanks"));
    assert_eq!(outside(&mut repo, lane), ["src/billing.ts:footer"]);
    write(&work, "src/billing.ts", &BILLING.replace("total", "sum"));
    assert_eq!(repo.integrate(lane).unwrap().result, Outcome::Integrated);
    assert!(read(&root, "src/billing.ts").contains("sum(items)"));
}

#[test]
fn directories_new_files_and_what_is_always_allowed() {
    let (_tmp, _root, mut repo) =
        shop_with("[scope]\nalways_allowed = [\"package-lock.json\", \"*.snap\"]\n");
    let (lane, work) = open(&mut repo, "alice", "a new module", &["src/reports/"]);
    write(&work, "src/reports/monthly.ts", "export function monthly(): number {\n  return 12;\n}\n");
    write(&work, "src/reports/notes.txt", "notes\n");
    write(&work, "package-lock.json", "{ \"changed\": true }\n");
    write(&work, "src/shop.test.snap", "snapshot\n");
    assert!(outside(&mut repo, lane).is_empty(), "{:?}", repo.out_of_scope(lane));
    write(&work, "src/types.ts", &format!("{TYPES}// more\n"));
    assert_eq!(outside(&mut repo, lane), ["src/types.ts (outside its functions)"]);

    // A function that does not exist yet is declared by the name it will have.
    let (later, work) = open(&mut repo, "bob", "a refund", &["src/shop.ts:refund"]);
    write(
        &work,
        "src/shop.ts",
        &format!("{SHOP}\nexport function refund(amount: number): number {{\n  return -amount;\n}}\n"),
    );
    assert!(outside(&mut repo, later).is_empty(), "{:?}", repo.out_of_scope(later));
}

#[test]
fn two_lanes_on_the_same_code_are_both_told_once() {
    let (_tmp, _root, mut repo) = shop();
    let (alice, alice_work) = open(&mut repo, "alice", "prices in cents", &["src/shop.ts:total"]);
    assert!(repo.take_notices(alice).unwrap().is_empty());

    // The same function: the second is told when it declares it, the first
    // at its next question, each once, with what the other is there for.
    let (bob, _) = open(&mut repo, "bob", "a discount over 50", &["src/shop.ts:total", "README.md"]);
    let heard = repo.take_notices(bob).unwrap();
    assert_eq!(told(&heard), [one("function", "lane/1", "src/shop.ts:total")]);
    assert_eq!(heard[0].agent, "alice");
    assert_eq!(heard[0].intent.as_deref(), Some("prices in cents"));
    assert!(heard[0].detail.contains("alice declared it"), "{}", heard[0].detail);
    assert_eq!(told(&repo.take_notices(alice).unwrap()), [one("function", "lane/2", "src/shop.ts:total")]);
    assert!(repo.take_notices(alice).unwrap().is_empty() && repo.take_notices(bob).unwrap().is_empty());

    // The same file, on another function; a directory against a file in it.
    let (carol, _) = open(&mut repo, "carol", "rounding", &["src/shop.ts:discount"]);
    assert_eq!(
        told(&repo.take_notices(carol).unwrap()),
        [one("file", "lane/1", "src/shop.ts"), one("file", "lane/2", "src/shop.ts"),]
    );
    let (dave, _) = open(&mut repo, "dave", "tidy the sources", &["src/"]);
    let heard = told(&repo.take_notices(dave).unwrap());
    assert!(
        heard.contains(&one("file", "lane/1", "src/shop.ts"))
            && heard.contains(&one("file", "lane/3", "src/shop.ts")),
        "{heard:?}"
    );
    let (erin, _) = open(&mut repo, "erin", "the billing", &["src/billing/", "src/"]);
    let heard = told(&repo.take_notices(erin).unwrap());
    assert!(heard.contains(&one("directory", "lane/4", "src/")), "{heard:?}");

    // Carol hears of the two who came after her at her next question.
    let heard = told(&repo.take_notices(carol).unwrap());
    assert_eq!(heard, [one("file", "lane/4", "src/shop.ts"), one("file", "lane/5", "src/shop.ts")]);

    // What a lane changes counts like what it declares: alice goes outside
    // her scope, into the function carol declared.
    write(&alice_work, "src/shop.ts", &SHOP.replace("Math.round(", "Math.ceil("));
    repo.scan_lane(alice).unwrap();
    let heard = repo.take_notices(carol).unwrap();
    assert_eq!(told(&heard), [one("function", "lane/1", "src/shop.ts:discount")]);
    assert!(heard[0].detail.contains("alice changed it"), "{}", heard[0].detail);
    // And alice is told she left her scope, once.
    let heard = repo.take_notices(alice).unwrap();
    assert!(heard.iter().any(|o| o.kind == "out_of_scope" && o.what == "src/shop.ts:discount"), "{heard:?}");
    assert!(heard.iter().any(|o| o.kind == "function" && o.lane == "lane/3"), "{heard:?}");
    assert!(repo.take_notices(alice).unwrap().is_empty());

    // A closed lane overlaps with nobody.
    repo.close_lane(bob).unwrap();
    assert!(repo.overlaps(alice).unwrap().iter().all(|o| o.lane != "lane/2"));
}

#[test]
fn a_function_that_uses_another_lanes_function_is_an_overlap() {
    let (_tmp, _root, mut repo) = shop();
    let (alice, _) = open(&mut repo, "alice", "prices in cents", &["src/shop.ts:total"]);
    // Bob has not written a line: the function he declared calls hers.
    let (bob, _) = open(&mut repo, "bob", "a clearer invoice", &["src/billing.ts:invoice"]);
    let heard = repo.take_notices(bob).unwrap();
    assert_eq!(told(&heard), [one("caller", "lane/1", "src/billing.ts:invoice uses src/shop.ts:total")]);
    assert!(heard[0]
        .detail
        .contains("your src/billing.ts:invoice uses src/shop.ts:total, which alice declared"));
    let heard = repo.take_notices(alice).unwrap();
    assert_eq!(told(&heard), [one("caller", "lane/2", "src/billing.ts:invoice uses src/shop.ts:total")]);
    assert!(heard[0]
        .detail
        .contains("bob's src/billing.ts:invoice uses src/shop.ts:total, which you declared"));
    // A function that uses nothing of hers is no overlap.
    let (carol, _) = open(&mut repo, "carol", "politer", &["src/billing.ts:footer"]);
    assert_eq!(told(&repo.take_notices(carol).unwrap()), [one("file", "lane/2", "src/billing.ts")]);
}

#[test]
fn the_board_and_who_touches_what() {
    let (_tmp, _root, mut repo) = shop_with("[scope]\nbroad_files = 3\n");
    let (alice, work) = open(&mut repo, "alice", "prices in cents", &["src/shop.ts:total"]);
    let (_bob, _) = open(&mut repo, "bob", "everything", &["./"]);
    write(&work, "src/shop.ts", &SHOP.replace("let sum = 0;", "let sum = 0; // cents"));
    write(&work, "README.md", "# changed\n");
    repo.scan_lane(alice).unwrap();

    let board = repo.board().unwrap();
    assert_eq!(board.len(), 2);
    let (a, b) = (&board[0], &board[1]);
    assert_eq!(
        (a.id.as_str(), a.agent.as_str(), a.intent.as_deref()),
        ("lane/1", "alice", Some("prices in cents"))
    );
    assert_eq!(a.scope[0].item, "src/shop.ts:total");
    assert_eq!((a.scope_files, a.broad), (1, false));
    assert_eq!(a.changed, ["README.md", "src/shop.ts:total"]);
    assert_eq!(a.out_of_scope, ["README.md"]);
    assert!(a.last_change_ms.is_some() && b.last_change_ms.is_none());
    assert!(a.overlaps.iter().all(|o| o.lane == "lane/2") && !a.overlaps.is_empty());
    // The whole repository is more than this one calls reasonable.
    assert!(b.broad && b.scope_files > 3 && b.changed.is_empty(), "{b:?}");

    let on_total = repo.who_touches("src/shop.ts:total").unwrap();
    assert_eq!(
        told(&on_total),
        [one("file", "lane/2", "src/shop.ts"), one("function", "lane/1", "src/shop.ts:total")]
    );
    assert!(on_total[1].detail.contains("alice declared and changed it"), "{}", on_total[1].detail);
    assert!(repo.who_touches("src/types.ts").unwrap().iter().all(|o| o.lane == "lane/2"));
    assert!(repo.who_touches("../x").is_err());
}

#[test]
fn a_repository_may_refuse_a_broad_scope() {
    let (_tmp, _root, mut repo) = shop_with("[scope]\nbroad_files = 2\nbroad = \"refuse\"\n");
    let refused = repo.open_lane("bob", None, None, "everything", &scope(&["./"])).unwrap_err().to_string();
    assert!(refused.contains("more than the 2 this repository allows"), "{refused}");
    assert!(repo.lanes(false).is_empty());
    let (lane, _) = open(&mut repo, "bob", "the shop", &["src/shop.ts"]);
    assert!(repo
        .extend_scope(lane, &scope(&["src/"]))
        .unwrap_err()
        .to_string()
        .contains("this scope covers"));
    assert_eq!(repo.lane_info(lane).unwrap().scope, ["src/shop.ts"]);
}

#[test]
fn a_lane_is_told_when_the_main_lane_changes_what_it_declared() {
    let (_tmp, _root, mut repo) = shop();
    let (alice, alice_work) = open(&mut repo, "alice", "prices in cents", &["src/shop.ts:total"]);
    let (bob, bob_work) = open(&mut repo, "bob", "a discount over 50", &["src/shop.ts:total", "README.md"]);
    repo.take_notices(alice).unwrap();
    repo.take_notices(bob).unwrap();

    // Bob's work reaches the main lane: alice still has the function as it
    // was, and is told who changed it and what for, once.
    write(&bob_work, "src/shop.ts", &SHOP.replace("return sum;", "return sum > 50 ? sum - 10 : sum;"));
    write(&bob_work, "README.md", "# Shop, with discounts\n");
    assert_eq!(repo.integrate(bob).unwrap().result, Outcome::Integrated);
    let heard = repo.take_notices(alice).unwrap();
    let landed: Vec<&Overlap> = heard.iter().filter(|o| o.kind == "landed").collect();
    assert_eq!(landed.len(), 1, "{heard:?}");
    assert_eq!((landed[0].what.as_str(), landed[0].agent.as_str()), ("src/shop.ts:total", "bob"));
    assert!(landed[0].lane.starts_with("main@"), "{}", landed[0].lane);
    assert!(landed[0].intent.as_deref().unwrap().contains("a discount over 50"), "{:?}", landed[0].intent);
    assert!(
        landed[0].detail.contains("bob changed src/shop.ts:total on the main lane"),
        "{}",
        landed[0].detail
    );
    assert!(repo.take_notices(alice).unwrap().iter().all(|o| o.kind != "landed"));
    // What she did not declare is not hers to hear about; and bob, who
    // continues from the new state, has nothing behind him.
    assert!(heard.iter().all(|o| o.what != "README.md"));
    assert!(repo.landed(bob).unwrap().is_empty());

    // Once her work is in, she is on the new state too.
    write(
        &alice_work,
        "src/shop.ts",
        &SHOP.replace("item.price * item.quantity", "(item.price * item.quantity) / 100"),
    );
    assert_eq!(repo.integrate(alice).unwrap().result, Outcome::Integrated);
    assert!(repo.landed(alice).unwrap().is_empty());
}
