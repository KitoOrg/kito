//! What an agent is held to before the fact: the why of what it declares,
//! edits refused outside a lane and outside its scope, an edit held once
//! for the reasons behind its lines, the same before an integration, and
//! what a shell command changed where it should not have.

use kito_core::paths::Canonical as _;
use kito_core::*;
use kito_integrate::Outcome;
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

const SHOP: &str = r#"export function total(prices: number[]): number {
  let sum = 0;
  for (const price of prices) {
    sum += price;
  }
  return sum;
}

export function discount(amount: number, percent: number): number {
  const cut = (amount * percent) / 100;
  return amount - cut;
}
"#;

const BILLING: &str = r#"import { total } from "./shop";

export function invoice(prices: number[]): string {
  return "to pay: " + total(prices);
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
    write(&root, "src/billing.ts", BILLING);
    write(&root, "README.md", "# Shop\n");
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn open(repo: &mut Repo, agent: &str, intent: &str, items: &[&str]) -> (LaneId, PathBuf) {
    let scope: Vec<String> = items.iter().map(|item| (*item).to_owned()).collect();
    let info = repo.open_lane(agent, None, None, intent, &scope).unwrap();
    (info.id.parse().unwrap(), info.workdir.unwrap())
}

/// The history of a small team: alice puts the prices in cents, in two
/// steps with a reason each; bob floors the discount.
fn team() -> (tempfile::TempDir, PathBuf, Repo) {
    let (tmp, root, mut repo) = shop();
    let (alice, work) = open(&mut repo, "alice", "prices in cents", &["src/shop.ts:total", "src/billing.ts"]);
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.round(price * 100);"));
    write(&work, "src/billing.ts", &BILLING.replace("total(prices)", "total(prices) / 100"));
    repo.scan_lane(alice).unwrap();
    repo.set_intent(alice, "no negative prices", Some("task #7".to_owned()), None).unwrap();
    let now = read(&work, "src/shop.ts");
    write(
        &work,
        "src/shop.ts",
        &now.replace(
            "let sum = 0;",
            "let sum = 0;\n  if (prices.some((p) => p < 0)) throw new Error(\"negative price\");",
        ),
    );
    assert_eq!(repo.integrate(alice).unwrap().result, Outcome::Integrated);
    repo.close_lane(alice).unwrap();

    let (bob, work) = open(&mut repo, "bob", "floor the discount", &["src/shop.ts:discount"]);
    let now = read(&work, "src/shop.ts");
    write(
        &work,
        "src/shop.ts",
        &now.replace("(amount * percent) / 100", "Math.floor((amount * percent) / 100)"),
    );
    assert_eq!(repo.integrate(bob).unwrap().result, Outcome::Integrated);
    repo.close_lane(bob).unwrap();
    (tmp, root, repo)
}

/// The lines of a file a text is on: what a hook finds before an edit.
fn lines_of(root: &Path, path: &str, text: &str) -> Vec<(u32, u32)> {
    let content = read(root, path);
    let at = content.find(text).unwrap_or_else(|| panic!("{text:?} is not in {path}"));
    let from = 1 + content[..at].matches('\n').count() as u32;
    vec![(from, from + text.trim_end_matches('\n').matches('\n').count() as u32)]
}

fn whys(reasons: &[kitod::why::Reason]) -> Vec<&str> {
    reasons.iter().map(|reason| reason.why.as_str()).collect()
}

#[test]
fn a_lane_is_told_why_what_it_declares_is_the_way_it_is() {
    let (_tmp, _root, mut repo) = team();
    let scope = ["src/shop.ts:total", "README.md", "src/", "src/new.ts:helper"].map(str::to_owned);
    let (carol, _) = open(&mut repo, "carol", "totals for several currencies", &["src/shop.ts:total"]);
    let (told, note) = repo.why_of_scope(carol, &scope);

    // The function: its reasons, newest first, and who uses it.
    let total = told.iter().find(|why| why.item == "src/shop.ts:total").expect("the function is told about");
    assert_eq!(whys(&total.reasons)[..2], ["no negative prices", "prices in cents"]);
    assert_eq!(total.reasons[0].by, "alice");
    assert_eq!(total.reasons[0].task.as_deref(), Some("task #7"));
    assert!(total.reasons[0].ops.is_empty(), "the short form");
    assert!(
        total.used_by.iter().any(|user| user.starts_with("src/billing.ts:invoice")),
        "{:?}",
        total.used_by
    );
    // The file. What does not exist yet has nothing to tell.
    assert!(told.iter().any(|why| why.item == "README.md"));
    assert!(!told.iter().any(|why| why.item.contains("new.ts")));
    // A directory is not told file by file.
    assert!(note.unwrap().contains("a directory was declared"));
}

#[test]
fn an_agent_does_not_edit_the_main_directory() {
    let (_tmp, root, mut repo) = team();
    let answer = repo.before_edit(None, Path::new("src/shop.ts"), &[(1, 2)], false).unwrap();
    assert!(!answer.allowed);
    assert_eq!(answer.kind, "main");
    assert!(answer.message.contains("open_lane"), "{}", answer.message);
    assert!(answer.message.contains("not where it is made"), "{}", answer.message);

    // A new file there is no better.
    assert_eq!(repo.before_edit(None, Path::new("src/new.ts"), &[], true).unwrap().kind, "main");
    // What Kito does not record is nobody's business.
    write(&root, ".gitignore", ".kito/\nbuild/\n");
    repo.scan().unwrap();
    assert!(repo.before_edit(None, Path::new("build/out.js"), &[], true).unwrap().allowed);

    // Protected, the answer says what would happen.
    repo.protect(true).unwrap();
    let answer = repo.before_edit(None, Path::new("src/shop.ts"), &[], true).unwrap();
    assert!(answer.message.contains("would be put back"), "{}", answer.message);

    // A lane that is not open is no place to work either.
    let gone = "lane/99".parse().unwrap();
    assert_eq!(repo.before_edit(Some(gone), Path::new("src/shop.ts"), &[], true).unwrap().kind, "closed");
}

#[test]
fn an_edit_outside_the_scope_is_refused_before_it_is_made() {
    let (_tmp, _root, mut repo) = team();
    let (carol, work) = open(&mut repo, "carol", "totals for several currencies", &["src/shop.ts:total"]);
    let scope = ["src/shop.ts:total".to_owned()];
    repo.why_of_scope(carol, &scope);
    let shop = Path::new("src/shop.ts");

    // Its own function: told why when it declared it, so nothing holds it.
    let own = lines_of(&work, "src/shop.ts", "  let sum = 0;\n");
    assert!(repo.before_edit(Some(carol), shop, &own, false).unwrap().allowed);
    // The text around the functions of a file follows them.
    assert!(repo.before_edit(Some(carol), shop, &[(8, 8)], false).unwrap().allowed);

    // Another function of the same file.
    let other = lines_of(&work, "src/shop.ts", "  return amount - cut;\n");
    let answer = repo.before_edit(Some(carol), shop, &other, false).unwrap();
    assert!(!answer.allowed);
    assert_eq!(answer.kind, "out_of_scope");
    assert_eq!(answer.outside, ["src/shop.ts:discount"]);
    assert!(answer.message.contains("extend_scope"), "{}", answer.message);
    assert!(answer.message.contains("[\"src/shop.ts:discount\"]"), "{}", answer.message);

    // Another file, and a file that does not exist yet.
    let answer = repo.before_edit(Some(carol), Path::new("src/billing.ts"), &[(3, 4)], false).unwrap();
    assert_eq!(
        (answer.kind.as_str(), answer.outside.as_slice()),
        ("out_of_scope", &["src/billing.ts".to_owned()][..])
    );
    assert_eq!(
        repo.before_edit(Some(carol), Path::new("src/tax.ts"), &[], true).unwrap().kind,
        "out_of_scope"
    );

    // Declared, the edit goes through: the answer to the declaration told why.
    repo.extend_scope(carol, &["src/shop.ts:discount".to_owned(), "src/tax.ts".to_owned()]).unwrap();
    let (told, _) = repo.why_of_scope(carol, &["src/shop.ts:discount".to_owned(), "src/tax.ts".to_owned()]);
    assert_eq!(whys(&told[0].reasons)[0], "floor the discount");
    assert!(repo.before_edit(Some(carol), shop, &other, false).unwrap().allowed);
    assert!(repo.before_edit(Some(carol), Path::new("src/tax.ts"), &[], true).unwrap().allowed);
}

#[test]
fn an_edit_is_held_once_for_the_reasons_behind_its_lines() {
    let (_tmp, root, mut repo) = team();
    // A directory: nothing was told when it was declared.
    let (dave, work) = open(&mut repo, "dave", "tidy the shop", &["src/"]);
    assert!(repo.why_of_scope(dave, &["src/".to_owned()]).0.is_empty());
    let shop = Path::new("src/shop.ts");

    let cut = lines_of(&work, "src/shop.ts", "  const cut = Math.floor((amount * percent) / 100);\n");
    let answer = repo.before_edit(Some(dave), shop, &cut, false).unwrap();
    assert!(!answer.allowed);
    assert_eq!(answer.kind, "why");
    assert_eq!(whys(&answer.reasons), ["floor the discount"]);
    assert!(
        answer.message.contains("- \"floor the discount\": bob for ada, lane/2, checkpoint ckpt#"),
        "{}",
        answer.message
    );
    assert!(answer.message.contains("make the same edit again"), "{}", answer.message);

    // The same edit again goes through, and so does anything else written for that reason.
    assert!(repo.before_edit(Some(dave), shop, &cut, false).unwrap().allowed);

    // Other lines, other reasons: two of alice's and the one that created the file.
    let sum = lines_of(
        &work,
        "src/shop.ts",
        "  let sum = 0;\n  if (prices.some((p) => p < 0)) throw new Error(\"negative price\");\n",
    );
    let answer = repo.before_edit(Some(dave), shop, &sum, false).unwrap();
    assert_eq!(whys(&answer.reasons), ["no negative prices", "start"]);
    assert!(repo.before_edit(Some(dave), shop, &sum, false).unwrap().allowed);

    // A file written whole: every reason behind it that was not told yet.
    let answer = repo.before_edit(Some(dave), shop, &[], true).unwrap();
    assert_eq!(whys(&answer.reasons), ["prices in cents"]);
    assert!(repo.before_edit(Some(dave), shop, &[], true).unwrap().allowed);

    // Its own work is no news to it.
    write(&work, "src/billing.ts", &read(&work, "src/billing.ts").replace("to pay: ", "due: "));
    let answer = repo.before_edit(Some(dave), Path::new("src/billing.ts"), &[(4, 4)], false).unwrap();
    assert!(answer.allowed, "the line is dave's own now, and he was told the rest: {answer:?}");

    // What a lane was told is kept with it: a restart does not tell it again.
    drop(repo);
    let mut repo = Repo::open(&root).unwrap();
    assert!(repo.before_edit(Some(dave), shop, &cut, false).unwrap().allowed);
    assert!(repo.before_edit(Some(dave), shop, &[], true).unwrap().allowed);

    // A repository may turn the holding off.
    let config = root.join(".kito/config.toml");
    fs::write(
        &config,
        format!(
            "{}\n[agents]\nwhy = false\n",
            fs::read_to_string(&config).unwrap().replace("[agents]", "[agents_before]")
        ),
    )
    .unwrap();
    repo.reload_config().unwrap();
    let (erin, work) = open(&mut repo, "erin", "more tidying", &["src/"]);
    let cut = lines_of(&work, "src/shop.ts", "  return amount - cut;\n");
    assert!(repo.before_edit(Some(erin), shop, &cut, false).unwrap().allowed);
    assert!(repo.reasons_behind(erin).unwrap().reasons.is_empty());
}

#[test]
fn a_lane_that_asked_why_was_told() {
    let (_tmp, _root, mut repo) = team();
    let (dave, work) = open(&mut repo, "dave", "tidy the shop", &["src/"]);
    // What the daemon does with the answer to `why` asked for a lane.
    let graph = repo.why_graph(dave, "src/shop.ts:discount", false).unwrap();
    repo.tell(dave, &graph.reasons);
    let cut = lines_of(&work, "src/shop.ts", "  const cut = Math.floor((amount * percent) / 100);\n");
    assert!(repo.before_edit(Some(dave), Path::new("src/shop.ts"), &cut, false).unwrap().allowed);
    // It asked about one function, not about the other.
    let sum = lines_of(&work, "src/shop.ts", "    sum += Math.round(price * 100);\n");
    let answer = repo.before_edit(Some(dave), Path::new("src/shop.ts"), &sum, false).unwrap();
    assert_eq!(whys(&answer.reasons), ["prices in cents"]);
}

#[test]
fn before_an_integration_the_reasons_behind_what_was_replaced() {
    let (_tmp, _root, mut repo) = team();
    let (erin, work) = open(&mut repo, "erin", "a flat discount", &["src/"]);
    // Changed with no hook in between: a shell, another editor, another agent.
    let now = read(&work, "src/shop.ts");
    write(&work, "src/shop.ts", &now.replace("Math.floor((amount * percent) / 100)", "percent"));
    // A new file replaces nothing.
    write(&work, "src/tax.ts", "export const rate = 22;\n");

    let behind = repo.reasons_behind(erin).unwrap();
    assert_eq!(behind.reasons.len(), 1, "{behind:?}");
    assert_eq!(behind.reasons[0].reason.why, "floor the discount");
    assert_eq!(behind.reasons[0].replaced_in, ["src/shop.ts"]);
    assert!(behind.message.contains("\"floor the discount\": bob"), "{}", behind.message);
    assert!(behind.message.contains("ask for the integration again"), "{}", behind.message);

    // Asked again, there is nothing left to tell: the integration goes ahead.
    assert!(repo.reasons_behind(erin).unwrap().reasons.is_empty());
    assert_eq!(repo.integrate(erin).unwrap().result, Outcome::Integrated);
    repo.close_lane(erin).unwrap();

    // An agent that was told at the edit is not told again at the integration.
    let (frank, work) = open(&mut repo, "frank", "sums start at zero", &["src/"]);
    let sum = lines_of(&work, "src/shop.ts", "  let sum = 0;\n");
    let answer = repo.before_edit(Some(frank), Path::new("src/shop.ts"), &sum, false).unwrap();
    assert_eq!(whys(&answer.reasons), ["start"]);
    write(&work, "src/shop.ts", &read(&work, "src/shop.ts").replace("let sum = 0;", "let sum: number = 0;"));
    assert!(repo.reasons_behind(frank).unwrap().reasons.is_empty());

    // The main lane has no lane to be told.
    assert!(repo.reasons_behind(LaneId::MAIN).is_err());
}

#[test]
fn after_a_command_what_it_changed_where_it_should_not_have() {
    let (_tmp, root, mut repo) = team();
    let since =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    assert!(repo.after_shell(since, &[], true).unwrap().message.is_empty(), "nothing happened");

    // A file of the main directory, changed by hand.
    write(&root, "README.md", "# Shop\n\nEdited where it stands.\n");
    repo.scan().unwrap();
    let report = repo.after_shell(since, &[], true).unwrap();
    assert_eq!(report.main.len(), 1);
    assert_eq!(
        (report.main[0].paths.as_slice(), report.main[0].put_back),
        (&["README.md".to_owned()][..], false)
    );
    assert!(
        report.message.contains("files of the main directory were changed: README.md"),
        "{}",
        report.message
    );
    assert!(report.message.contains("undo it"), "{}", report.message);
    // What happened before the command is not its doing.
    assert!(repo.after_shell(since + 3_600_000, &[], true).unwrap().message.is_empty());

    // Protected, it was put back.
    repo.protect(true).unwrap();
    let since =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    write(&root, "src/shop.ts", "export const nothing = 0;\n");
    repo.scan().unwrap();
    let report = repo.after_shell(since, &[], true).unwrap();
    assert_eq!(report.main.len(), 1, "{report:?}");
    assert!(report.main[0].put_back && report.main[0].kept.is_some());
    assert!(report.message.contains("put back as they were"), "{}", report.message);
    assert!(read(&root, "src/shop.ts").contains("export function total"));

    // An integration writes the main directory too: that is not by hand.
    let (gina, work) = open(&mut repo, "gina", "a title", &["README.md"]);
    let since =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    write(&work, "README.md", "# The shop\n");
    assert_eq!(repo.integrate(gina).unwrap().result, Outcome::Integrated);
    assert!(repo.after_shell(since, &[gina], true).unwrap().message.is_empty());

    // A lane changed outside what it declared: looked at now, not when a watcher gets to it.
    write(&work, "src/billing.ts", &read(&work, "src/billing.ts").replace("to pay: ", "due: "));
    let report = repo.after_shell(since, &[gina], true).unwrap();
    assert_eq!(report.lanes.len(), 1, "{report:?}");
    assert_eq!(report.lanes[0].out_of_scope, ["src/billing.ts:invoice"]);
    assert!(report.message.contains("lane/"), "{}", report.message);
    assert!(report.message.contains("extend_scope"), "{}", report.message);
}

#[test]
fn the_first_import_is_nobodys_edit_and_an_edit_by_hand_is_told_once() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    drop(Repo::init(&root).unwrap());
    // Whatever was recorded counts as the doing of the command that asks:
    // what is told here does not hang on the clock.
    let since = 0;
    // A project that was there before Kito: its files, written a moment ago.
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    write(&root, "README.md", "# Shop\n");
    // What a daemon does when it starts: it takes them in.
    let mut repo = Repo::open(&root).unwrap();
    assert!(!repo.scan_at_start().unwrap().is_empty());
    let report = repo.after_shell(since, &[], true).unwrap();
    assert!(report.main.is_empty() && report.message.is_empty(), "{report:?}");

    // An edit made by hand afterwards is told to a session that asks, once.
    write(&root, "README.md", "# Shop\n\nEdited where it stands.\n");
    repo.scan().unwrap();
    let report = repo.after_shell_for(since, &[], true, Some("one")).unwrap();
    assert_eq!(report.main.len(), 1, "{report:?}");
    assert!(
        report.message.contains("files of the main directory were changed: README.md."),
        "{}",
        report.message
    );
    assert!(repo.after_shell_for(since, &[], true, Some("one")).unwrap().message.is_empty());
    // Another session has not heard of it; nor has one that gives no name.
    assert_eq!(repo.after_shell_for(since, &[], true, Some("two")).unwrap().main.len(), 1);
    assert!(repo.after_shell_for(since, &[], true, Some("two")).unwrap().main.is_empty());
    assert_eq!(repo.after_shell(since, &[], true).unwrap().main.len(), 1);
    assert!(repo.after_shell(since, &[], true).unwrap().main.is_empty());
    // The next edit of the same file is another edit.
    write(&root, "README.md", "# Shop\n\nEdited again.\n");
    repo.scan().unwrap();
    assert_eq!(repo.after_shell_for(since, &[], true, Some("one")).unwrap().main.len(), 1);
    assert!(repo.after_shell_for(since, &[], true, Some("one")).unwrap().main.is_empty());

    // A daemon that starts on a repository with a history: what it finds
    // changed was changed by hand while it was away.
    drop(repo);
    write(&root, "src/shop.ts", "export const nothing = 0;\n");
    let mut repo = Repo::open(&root).unwrap();
    repo.scan_at_start().unwrap();
    let report = repo.after_shell(since, &[], true).unwrap();
    assert_eq!(report.main.len(), 1, "{report:?}");
    assert_eq!(report.main[0].paths, ["src/shop.ts"]);
}
