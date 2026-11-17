//! Why code is the way it is: the reasons of a function, of a file and of
//! lines, what else changed for a reason, who uses what, and the search.

use kito_core::*;
use kito_integrate::Outcome;
use kitod::why::WhyGraph;
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
    let root = tmp.path().canonicalize().unwrap();
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

/// The reasons of an answer, as (why, how, checkpoint).
fn told(answer: &WhyGraph) -> Vec<(String, String, Option<String>)> {
    answer.reasons.iter().map(|r| (r.why.clone(), r.how.join(", "), r.checkpoint.clone())).collect()
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

#[test]
fn the_reasons_of_a_function_and_of_each_of_its_lines() {
    let (_tmp, root, mut repo) = team();
    let answer = repo.why_graph(LaneId::MAIN, "src/shop.ts:total", false).unwrap();
    assert_eq!(answer.target.kind, "function");
    assert_eq!(answer.target.name.as_deref(), Some("total"));
    assert_eq!(answer.target.lines, Some((1, 8)));

    // Newest first: the two reasons alice gave in her lane, each with the
    // checkpoint that brought it in, then how the function came to be.
    assert_eq!(
        told(&answer),
        [
            ("no negative prices".to_owned(), "body changed".to_owned(), Some("ckpt#2".to_owned())),
            ("prices in cents".to_owned(), "body changed".to_owned(), Some("ckpt#2".to_owned())),
            ("start".to_owned(), "created".to_owned(), Some("ckpt#1".to_owned())),
        ]
    );
    let (negative, cents, start) = (&answer.reasons[0], &answer.reasons[1], &answer.reasons[2]);
    assert_eq!(
        (negative.by.as_str(), negative.person.as_deref(), negative.lane.as_deref()),
        ("alice", Some("ada"), Some("lane/1"))
    );
    assert_eq!(negative.task.as_deref(), Some("task #7"));
    assert_eq!((start.by.as_str(), start.lane.as_deref()), ("ada", None));
    // What else changed for the same reason: the caller she adapted, with
    // the first reason and not with the second.
    assert_eq!(cents.with, ["src/billing.ts:invoice"]);
    assert!(negative.with.is_empty(), "{:?}", negative.with);
    // What came with the first checkpoint: everything that was there.
    assert!(
        start.with.contains(&"src/shop.ts:discount".to_owned())
            && start.with.contains(&"README.md".to_owned())
    );

    // Each line, with the reason it was last written for.
    let text = read(&root, "src/shop.ts");
    let line = |needle: &str| 1 + text.lines().position(|l| l.contains(needle)).unwrap() as u32;
    let reason_at = |n: u32| {
        let run = answer.lines.iter().find(|run| run.from <= n && n <= run.to).unwrap();
        run.reason.map(|index| answer.reasons[index].why.as_str())
    };
    assert_eq!(reason_at(line("export function total")), Some("start"));
    assert_eq!(reason_at(line("negative price")), Some("no negative prices"));
    assert_eq!(reason_at(line("Math.round(price * 100)")), Some("prices in cents"));
    assert_eq!(reason_at(line("return sum;")), Some("start"));
    assert_eq!(answer.lines.first().unwrap().from, 1);
    assert_eq!(answer.lines.last().unwrap().to, 8);

    // Who uses it, with the last reason that one changed for.
    assert_eq!(answer.used_by.len(), 1, "{:?}", answer.used_by);
    let user = &answer.used_by[0];
    assert_eq!((user.symbol.as_str(), user.line), ("src/billing.ts:invoice", Some(4)));
    assert_eq!(user.last.as_ref().unwrap().why, "prices in cents");
    assert!(answer.uses.is_empty() && answer.in_progress.is_empty() && answer.decisions.is_empty());

    // The other way round: what a function uses.
    let invoice = repo.why_graph(LaneId::MAIN, "src/billing.ts:invoice", false).unwrap();
    assert_eq!(invoice.uses.len(), 1);
    assert_eq!(invoice.uses[0].symbol, "src/shop.ts:total");
    assert_eq!(invoice.uses[0].last.as_ref().unwrap().why, "no negative prices");
}

#[test]
fn lines_files_and_what_is_asked_wrongly() {
    let (_tmp, root, mut repo) = team();
    let text = read(&root, "src/shop.ts");
    let line = |needle: &str| 1 + text.lines().position(|l| l.contains(needle)).unwrap() as u32;

    // A line says which function it is in; only the lines asked are told.
    let asked = line("Math.floor");
    let answer = repo.why_graph(LaneId::MAIN, &format!("src/shop.ts:{asked}"), false).unwrap();
    assert_eq!(answer.target.name.as_deref(), Some("discount"));
    assert_eq!(answer.lines.len(), 1);
    assert_eq!((answer.lines[0].from, answer.lines[0].to), (asked, asked));
    assert_eq!(answer.reasons[answer.lines[0].reason.unwrap()].why, "floor the discount");
    assert_eq!(answer.reasons[0].by, "bob");
    let range =
        repo.why_graph(LaneId::MAIN, &format!("src/shop.ts:{}-{}", asked - 1, asked + 1), false).unwrap();
    assert_eq!(range.lines.iter().map(|run| run.to - run.from + 1).sum::<u32>(), 3);

    // A file: what it went through as a whole, and what it contains.
    let file = repo.why_graph(LaneId::MAIN, "src/shop.ts", false).unwrap();
    assert_eq!(file.target.kind, "file");
    let whys: Vec<(String, String)> = told(&file).into_iter().map(|(why, how, _)| (why, how)).collect();
    let changed = |why: &str| (why.to_owned(), "changed".to_owned());
    assert_eq!(
        whys,
        [
            changed("floor the discount"),
            changed("no negative prices"),
            changed("prices in cents"),
            ("start".to_owned(), "created".to_owned())
        ]
    );
    let contains: Vec<(&str, &str)> =
        file.contains.iter().map(|n| (n.symbol.as_str(), n.last.as_ref().unwrap().why.as_str())).collect();
    assert_eq!(
        contains,
        [("src/shop.ts:total", "no negative prices"), ("src/shop.ts:discount", "floor the discount")]
    );
    // A file with no functions has its own reasons, and that is all.
    let readme = repo.why_graph(LaneId::MAIN, "README.md", false).unwrap();
    assert_eq!(told(&readme).len(), 1);
    assert!(readme.contains.is_empty());
    // A line outside every function is the file's: the reason it was
    // written for, and nothing else.
    let import = repo.why_graph(LaneId::MAIN, "src/billing.ts:1", false).unwrap();
    assert_eq!((import.target.kind.as_str(), import.target.lines), ("file", Some((1, 1))));
    assert_eq!(
        told(&import),
        [("start".to_owned(), "wrote lines of it".to_owned(), Some("ckpt#1".to_owned()))]
    );
    assert_eq!(import.lines.len(), 1);
    assert_eq!((import.lines[0].from, import.lines[0].to, import.lines[0].reason), (1, 1, Some(0)));
    // The lines of a whole file, by reason: every line has one.
    assert_eq!(file.lines.first().unwrap().from, 1);
    assert_eq!(file.lines.last().unwrap().to, text.lines().count() as u32);
    assert!(file.lines.iter().all(|run| run.reason.is_some()), "{:?}", file.lines);
    let floor = file.lines.iter().find(|run| run.from <= asked && asked <= run.to).unwrap();
    assert_eq!(file.reasons[floor.reason.unwrap()].why, "floor the discount");
    assert_eq!(readme.lines.len(), 1);

    let said =
        |target: &str, repo: &mut Repo| repo.why_graph(LaneId::MAIN, target, false).unwrap_err().to_string();
    assert!(said("src/shop.ts:nothing", &mut repo).contains("the file has: discount, total"));
    assert!(said("src/missing.ts", &mut repo).contains("no such file"));
    assert!(said(" ", &mut repo).contains("ask about a function"));
}

#[test]
fn work_in_progress_decisions_and_the_view_from_a_lane() {
    let (_tmp, _root, mut repo) = team();
    // Carol is on `total` now, and has changed it in her lane.
    let (carol, work) = open(&mut repo, "carol", "taxes on top", &["src/shop.ts:total"]);
    let now = read(&work, "src/shop.ts");
    write(&work, "src/shop.ts", &now.replace("return sum;", "return Math.round(sum * 1.22);"));

    // From the main lane: her reason is not history yet, but she is there.
    let main = repo.why_graph(LaneId::MAIN, "src/shop.ts:total", false).unwrap();
    assert_eq!(main.reasons[0].why, "no negative prices");
    assert_eq!(main.in_progress.len(), 1, "{:?}", main.in_progress);
    let there = &main.in_progress[0];
    assert_eq!(
        (there.lane.as_str(), there.agent.as_str(), there.intent.as_deref()),
        ("lane/3", "carol", Some("taxes on top"))
    );

    // From her lane: her change is the newest reason, in no checkpoint, and
    // the line is hers.
    let hers = repo.why_graph(carol, "src/shop.ts:total", false).unwrap();
    assert_eq!(told(&hers)[0], ("taxes on top".to_owned(), "body changed".to_owned(), None));
    assert_eq!(hers.reasons.len(), 4);
    let taxed = hers.lines.iter().find(|run| run.reason == Some(0)).unwrap();
    assert_eq!(taxed.from, taxed.to);
    assert!(hers.in_progress.is_empty(), "a lane is not told of itself");

    // Dave changes the same line another way, and gets in first: carol's
    // integration finds a conflict, she decides, and the decision is part
    // of what the function went through.
    let (dave, work) = open(&mut repo, "dave", "totals as integers", &["src/shop.ts:total"]);
    let now = read(&work, "src/shop.ts");
    write(&work, "src/shop.ts", &now.replace("return sum;", "return Math.trunc(sum);"));
    assert_eq!(repo.integrate(dave).unwrap().result, Outcome::Integrated);
    let stopped = repo.integrate(carol).unwrap();
    assert_eq!(stopped.result, Outcome::Conflicts, "{stopped:?}");
    let conflict: ConflictId = stopped.conflicts[0].id.parse().unwrap();
    repo.resolve_conflict(conflict, kitod::integrate::Decision::Lane).unwrap();
    assert_eq!(repo.integrate(carol).unwrap().result, Outcome::Integrated);
    let after = repo.why_graph(LaneId::MAIN, "src/shop.ts:total", false).unwrap();
    assert_eq!(after.decisions.len(), 1, "{:?}", after.decisions);
    let decision = &after.decisions[0];
    assert_eq!(
        (decision.decided.as_deref(), decision.applied, decision.lane.as_deref()),
        (Some("lane"), true, Some("lane/3"))
    );
    let whys: Vec<&str> = after.reasons.iter().map(|r| r.why.as_str()).collect();
    assert_eq!(
        whys,
        ["taxes on top", "totals as integers", "no negative prices", "prices in cents", "start"]
    );
    // All of this, lanes opened and integrated between the questions
    // included, with the log read once.
    assert_eq!(repo.why_index_builds(), 1);
}

#[test]
fn reasons_are_found_by_their_words() {
    let (_tmp, _root, mut repo) = team();
    let hits = repo.reasons("cents", 10).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let hit = &hits[0];
    assert_eq!(
        (hit.why.as_str(), hit.lane.as_deref(), hit.checkpoint.as_deref()),
        ("prices in cents", Some("lane/1"), Some("ckpt#2"))
    );
    assert_eq!(hit.changed, ["src/billing.ts:invoice", "src/shop.ts:total"]);
    assert_eq!((hit.by.as_slice(), hit.person.as_deref()), (&["alice".to_owned()][..], Some("ada")));

    // By the agent, by the task, by several words in any order; newest first.
    let alices: Vec<String> = repo.reasons("alice", 10).unwrap().into_iter().map(|h| h.why).collect();
    assert_eq!(alices, ["no negative prices", "prices in cents"]);
    assert_eq!(repo.reasons("task #7", 10).unwrap()[0].why, "no negative prices");
    assert_eq!(repo.reasons("discount the FLOOR", 10).unwrap()[0].why, "floor the discount");
    assert!(repo.reasons("nothing like this", 10).unwrap().is_empty());
    // Everything, with no words: what Kito did by itself is not in it.
    let all: Vec<String> = repo.reasons("", 10).unwrap().into_iter().map(|h| h.why).collect();
    assert_eq!(all, ["floor the discount", "no negative prices", "prices in cents", "start"]);
    assert_eq!(repo.reasons("", 2).unwrap().len(), 2);
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Grace")
        .env("GIT_AUTHOR_EMAIL", "grace@example.com")
        .env("GIT_COMMITTER_NAME", "Grace")
        .env("GIT_COMMITTER_EMAIL", "grace@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn code_older_than_kito_has_the_reasons_git_recorded() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    // A repository with a history of its own, before Kito.
    write(&root, "src/shop.ts", SHOP);
    write(&root, "src/billing.ts", BILLING);
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "a shop that adds up prices"]);
    write(&root, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.max(price, 0);"));
    git(&root, &["commit", "-q", "-am", "negative prices count as nothing"]);
    write(
        &root,
        "src/shop.ts",
        &read(&root, "src/shop.ts").replace("return amount - cut;", "return Math.max(amount - cut, 0);"),
    );
    git(&root, &["commit", "-q", "-am", "a discount never goes below zero"]);
    fs::write(root.join(".git/info/exclude"), ".kito/\n").unwrap();
    let mut repo = Repo::init(&root).unwrap();
    repo.import_git().unwrap();
    repo.scan().unwrap();
    repo.analyze().unwrap();

    // Kito never saw `total` being written: its lines say which commits
    // wrote them, and those are its reasons. The commit that only touched
    // another function is not one of them.
    let answer = repo.why_graph(LaneId::MAIN, "src/shop.ts:total", false).unwrap();
    let whys: Vec<(&str, &str, &str)> =
        answer.reasons.iter().map(|r| (r.why.as_str(), r.how[0].as_str(), r.by.as_str())).collect();
    assert_eq!(
        whys,
        [
            ("negative prices count as nothing", "wrote lines of it", "Grace"),
            ("a shop that adds up prices", "wrote lines of it", "Grace"),
        ]
    );
    let changed = answer.lines.iter().find(|run| run.reason == Some(0)).unwrap();
    assert_eq!((changed.from, changed.to), (4, 4));
    assert!(answer.lines.iter().all(|run| run.reason.is_some()), "{:?}", answer.lines);
    assert!(answer.notes.is_empty(), "{:?}", answer.notes);
    assert_eq!(answer.reasons[1].checkpoint.as_deref(), Some("ckpt#1"));
    // Who uses it is known as for any other function, with the reason of
    // the file it is in when it has none of its own.
    assert_eq!(answer.used_by[0].symbol, "src/billing.ts:invoice");
    assert_eq!(answer.used_by[0].last.as_ref().unwrap().why, "a shop that adds up prices");

    // From now on reasons come from lanes, next to the ones git had.
    let (lane, work) = open(&mut repo, "alice", "prices in cents", &["src/shop.ts:total"]);
    write(
        &work,
        "src/shop.ts",
        &read(&work, "src/shop.ts").replace("Math.max(price, 0)", "Math.max(Math.round(price * 100), 0)"),
    );
    assert_eq!(repo.integrate(lane).unwrap().result, Outcome::Integrated);
    let after = repo.why_graph(LaneId::MAIN, "src/shop.ts:total", false).unwrap();
    let whys: Vec<&str> = after.reasons.iter().map(|r| r.why.as_str()).collect();
    assert_eq!(
        whys,
        ["prices in cents", "a shop that adds up prices"],
        "the commit whose line was rewritten wrote none of it now"
    );
    assert_eq!(after.reasons[0].how, ["body changed"]);
    let found: Vec<String> = repo.reasons("prices", 10).unwrap().into_iter().map(|hit| hit.why).collect();
    assert_eq!(found, ["prices in cents", "negative prices count as nothing", "a shop that adds up prices"]);
}

#[test]
fn what_is_edited_by_hand_takes_the_reason_of_its_checkpoint() {
    let (_tmp, root, mut repo) = team();
    assert_eq!(repo.reasons("", 1).unwrap()[0].why, "floor the discount");
    // Somebody edits the main directory by hand: until a checkpoint takes
    // it in, the change has no reason to show.
    write(&root, "README.md", "# Shop\n\nOpen every day.\n");
    repo.scan().unwrap();
    let before = repo.why_graph(LaneId::MAIN, "README.md", false).unwrap();
    assert!(before.reasons[0].why.contains("no reason was recorded"), "{:?}", before.reasons);
    assert_eq!(before.reasons[0].checkpoint, None);
    // The checkpoint's label is its reason from then on, without reading
    // the whole log again.
    repo.checkpoint("opening hours in the README").unwrap();
    let after = repo.why_graph(LaneId::MAIN, "README.md", false).unwrap();
    let whys: Vec<&str> = after.reasons.iter().map(|r| r.why.as_str()).collect();
    assert_eq!(whys, ["opening hours in the README", "start"]);
    assert_eq!(after.reasons[0].checkpoint.as_deref(), Some("ckpt#4"));
    assert_eq!(after.lines.last().unwrap().reason, Some(0));
    assert_eq!(repo.reasons("opening hours", 5).unwrap()[0].changed, ["README.md"]);
    assert_eq!(repo.why_index_builds(), 1);
}
