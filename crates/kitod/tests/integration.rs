//! Integration, without the daemon: lanes become part of the main lane.

use kito_core::paths::Canonical as _;
use kito_core::*;
use kito_integrate::Outcome;
use kitod::integrate::Decision;
use kitod::Repo;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
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

const SHOP: &str = r#"import { db } from "./db";

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

/// A repository with one checkpoint, and its root.
fn shop() -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let mut repo = Repo::init(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    write(&root, "README.md", "# Shop\n");
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn workdir(repo: &Repo, lane: u64) -> PathBuf {
    repo.lane_info(LaneId(lane)).unwrap().workdir.unwrap()
}

#[test]
fn two_agents_on_the_same_file_integrate_without_conflicts() {
    let (_tmp, root, mut repo) = shop();
    repo.open_lane("alice", Some("cents everywhere".into()), None, "work", &kitod::scope::everything())
        .unwrap();
    repo.open_lane("bob", Some("receipts".into()), None, "work", &kitod::scope::everything()).unwrap();
    let (alice, bob) = (workdir(&repo, 1), workdir(&repo, 2));
    let total = repo.symbols().find(Path::new("src/shop.ts"), "total")[0];

    // Alice edits one function; Bob edits the other and adds a third at the
    // end of the same file.
    write(&alice, "src/shop.ts", &SHOP.replace("let sum = 0;", "let sum = 0; // cents"));
    let receipt = "\nexport function receipt(items: Item[]): string {\n  return items.map((item) => item.name).join(\", \");\n}\n";
    write(&bob, "src/shop.ts", &(SHOP.replace("Math.round(", "Math.floor(") + receipt));

    let first = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(first.result, Outcome::Integrated, "{first:?}");
    assert_eq!(
        (first.checkpoint.as_deref(), first.your_next_base.as_deref()),
        (Some("ckpt#2"), Some("ckpt#2"))
    );
    assert!(first.conflicts.is_empty() && first.git_commit.is_some());
    let second = repo.integrate(LaneId(2)).unwrap();
    assert_eq!(
        (second.result, second.checkpoint.as_deref()),
        (Outcome::Integrated, Some("ckpt#3")),
        "{second:?}"
    );

    // The main working directory has both, and so does the log.
    let merged = read(&root, "src/shop.ts");
    for needle in ["let sum = 0; // cents", "Math.floor(", "export function receipt"] {
        assert!(merged.contains(needle), "{needle} is missing from:\n{merged}");
    }
    assert!(repo.scan().unwrap().is_empty(), "what was written is what was recorded");
    // Symbols are the same ones: the edited function keeps its ID, the new
    // one has the ID Bob's lane gave it.
    assert_eq!(repo.symbols().find(Path::new("src/shop.ts"), "total"), [total]);
    let added = repo.symbols().find(Path::new("src/shop.ts"), "receipt")[0];
    assert_eq!(added.origin(), LaneId(2));

    // The ops on the main lane are signed by the agents and say why; the
    // first one of each integration also hangs from the lane's head.
    let ops = repo.store().lane_ops(LaneId::MAIN, 0, 100).unwrap();
    let by_alice: Vec<&Op> = ops.iter().filter(|op| op.author == Author::agent("alice")).collect();
    // (A comment changes the file, not the function: no symbolic op.)
    assert_eq!(by_alice.iter().map(|op| op.kind.name()).collect::<Vec<_>>(), ["TextEdit"]);
    assert_eq!(by_alice[0].parents.len(), 2);
    assert_eq!(by_alice[0].parents[1], repo.store().lane_ops(LaneId(1), 0, 10).unwrap().last().unwrap().id);
    let why = repo.store().intent(&by_alice[0].intent.unwrap()).unwrap();
    assert_eq!(why.summary, "integrate lane/1: cents everywhere");

    // Each integration is a checkpoint by its agent, and a commit in git.
    let log = git(&root, &["log", "--format=%an|%s"]);
    assert_eq!(log.lines().collect::<Vec<_>>(), ["bob|receipts", "alice|cents everywhere", "ada|start"]);
    assert_eq!(git(&root, &["status", "--porcelain"]), "");
    git(&root, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(git(&root, &["show", "HEAD:src/shop.ts"]), merged);

    // Each lane now continues from the state it was integrated into.
    assert_eq!(read(&alice, "src/shop.ts"), SHOP.replace("let sum = 0;", "let sum = 0; // cents"));
    assert_eq!(read(&bob, "src/shop.ts"), merged);
    let info = repo.lane_info(LaneId(2)).unwrap();
    assert_eq!(
        (info.base.as_str(), info.ops, info.files.len(), info.state.as_str()),
        ("ckpt#3", 0, 0, "active")
    );
    assert!(repo.scan_lane(LaneId(1)).unwrap().is_empty() && repo.scan_lane(LaneId(2)).unwrap().is_empty());

    // Asking again with nothing new: the lane just catches up.
    let again = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(
        (again.result, again.your_next_base.as_deref()),
        (Outcome::NothingToIntegrate, Some("ckpt#3"))
    );
    assert_eq!(read(&alice, "src/shop.ts"), merged);
    assert!(repo.store().verify().unwrap().is_ok());
}

#[test]
fn a_lane_keeps_working_after_being_integrated_and_survives_a_restart() {
    let (_tmp, root, mut repo) = shop();
    repo.open_lane("alice", None, None, "work", &kitod::scope::everything()).unwrap();
    let alice = workdir(&repo, 1);

    write(&alice, "src/shop.ts", &SHOP.replace("let sum = 0;", "let sum = 0; // cents"));
    assert_eq!(repo.integrate(LaneId(1)).unwrap().result, Outcome::Integrated);
    // Meanwhile someone saves on the main lane, without a checkpoint. An
    // integration looks at the main directory only in the files involved:
    // the rest is captured as it is saved (here, as the watcher would).
    write(&root, "README.md", "# Shop\n\nNow with cents.\n");
    repo.scan().unwrap();
    // Second round of the same lane, on a function it creates now.
    let helper = "\nexport function cents(amount: number): string {\n  return `${amount} c`;\n}\n";
    write(&alice, "src/shop.ts", &(read(&alice, "src/shop.ts") + helper));
    repo.scan_lane(LaneId(1)).unwrap();
    let born = repo.lane_info(LaneId(1)).unwrap();
    assert_eq!((born.ops, born.files.clone()), (2, vec!["src/shop.ts".to_owned()]));

    // A restart in the middle, from the log alone.
    repo.close().unwrap();
    fs::remove_file(root.join(".kito/lanes/1/state")).unwrap();
    fs::remove_file(root.join(".kito/lanes/main/state")).unwrap();
    let mut repo = Repo::open(&root).unwrap();
    assert_eq!(repo.lane_info(LaneId(1)).unwrap(), born);

    let result = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(result.result, Outcome::Integrated, "{result:?}");
    let merged = read(&root, "src/shop.ts");
    assert!(merged.contains("// cents") && merged.contains("export function cents"), "{merged}");
    assert_eq!(read(&root, "README.md"), "# Shop\n\nNow with cents.\n");
    // The lane caught up with what the main lane had in the meantime.
    assert_eq!(read(&alice, "README.md"), "# Shop\n\nNow with cents.\n");
    // The symbol born in the second round has the lane's first ID; one born
    // later continues the numbering, even after another restart.
    let cents = repo.symbols().find(Path::new("src/shop.ts"), "cents")[0];
    assert_eq!(cents, SymbolId::new(LaneId(1), 0));
    repo.close().unwrap();
    fs::remove_file(root.join(".kito/lanes/1/state")).unwrap();
    let mut repo = Repo::open(&root).unwrap();
    write(&alice, "src/more.ts", "export function more(): number {\n  return 2;\n}\n");
    repo.scan_lane(LaneId(1)).unwrap();
    assert_eq!(repo.integrate(LaneId(1)).unwrap().result, Outcome::Integrated);
    assert_eq!(repo.symbols().find(Path::new("src/more.ts"), "more"), [SymbolId::new(LaneId(1), 1)]);

    // The whole story of the function, across the lane and the main lane.
    let story: Vec<String> = repo.why(Path::new("src/shop.ts"), "cents").unwrap()[0]
        .steps
        .iter()
        .map(|s| s.what.clone())
        .collect();
    assert_eq!(
        story,
        ["function `cents` created in src/shop.ts, on lane/1", "integrated into the main lane"]
    );
    git(&root, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(git(&root, &["show", "HEAD:src/shop.ts"]), merged);
}

#[test]
fn a_conflict_blocks_the_lane_until_it_is_decided() {
    let (_tmp, root, mut repo) = shop();
    repo.open_lane("alice", None, None, "work", &kitod::scope::everything()).unwrap();
    let alice = workdir(&repo, 1);
    let ops_before = repo.status().unwrap().ops;

    // The same line of the same function, two ways; and another function,
    // changed only on the lane.
    write(&root, "src/shop.ts", &SHOP.replace("return sum;", "return Math.max(sum, 0);"));
    write(
        &alice,
        "src/shop.ts",
        &SHOP.replace("return sum;", "return Math.abs(sum);").replace("Math.round(", "Math.floor("),
    );
    let blocked = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(blocked.result, Outcome::Conflicts);
    assert_eq!(blocked.conflicts.len(), 1);
    let conflict = &blocked.conflicts[0];
    assert_eq!(
        (conflict.id.as_str(), conflict.kind.as_str(), conflict.lane.as_str()),
        ("conflict#1", "body_body", "lane/1")
    );
    assert_eq!((conflict.symbol.as_deref(), conflict.file.as_deref()), (Some("total"), Some("src/shop.ts")));
    assert!(conflict.main.as_deref().unwrap().contains("Math.max(sum, 0)"));
    assert!(conflict.lane_text.as_deref().unwrap().contains("Math.abs(sum)"));
    assert!(conflict.base.as_deref().unwrap().contains("return sum;"));
    // Nothing reached the main lane but the save that was pending there.
    assert_eq!(repo.status().unwrap().ops, ops_before + 2);
    assert!(!read(&root, "src/shop.ts").contains("Math.floor"));
    let info = repo.lane_info(LaneId(1)).unwrap();
    assert_eq!((info.state.as_str(), info.conflicts), ("blocked", vec!["conflict#1".to_owned()]));
    assert_eq!(repo.conflicts(false), blocked.conflicts);

    // Asking again changes nothing: same conflict, same number.
    let again = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(again.conflicts, blocked.conflicts);

    // Decided with a text of its own; the lane may ask again.
    let written = "export function total(items: Item[]): number {\n  let sum = 0;\n  for (const item of items) {\n    sum += item.price * item.quantity;\n  }\n  return Math.max(Math.abs(sum), 0);\n}\n";
    let decided = repo.resolve_conflict(ConflictId(1), Decision::Text(written.to_owned())).unwrap();
    assert_eq!((decided.resolution.as_deref(), decided.resolved), (Some("text"), false));
    assert_eq!(repo.lane_info(LaneId(1)).unwrap().state, "ready");
    assert!(repo
        .resolve_conflict(ConflictId(9), Decision::Main)
        .unwrap_err()
        .to_string()
        .contains("does not exist"));

    let done = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    let merged = read(&root, "src/shop.ts");
    assert!(
        merged.contains("return Math.max(Math.abs(sum), 0);") && merged.contains("Math.floor("),
        "{merged}"
    );
    assert_eq!(read(&alice, "src/shop.ts"), merged);
    assert!(repo.conflicts(false).is_empty());
    let settled = &repo.conflicts(true)[0];
    assert!(settled.resolved && settled.resolution.as_deref() == Some("text"));
    assert!(repo
        .resolve_conflict(ConflictId(1), Decision::Main)
        .unwrap_err()
        .to_string()
        .contains("already settled"));
    assert_eq!(repo.lane_info(LaneId(1)).unwrap().state, "active");
    assert!(repo.scan().unwrap().is_empty() && repo.scan_lane(LaneId(1)).unwrap().is_empty());
}

#[test]
fn a_decision_holds_only_for_the_texts_it_was_taken_on() {
    let (_tmp, root, mut repo) = shop();
    repo.open_lane("alice", None, None, "work", &kitod::scope::everything()).unwrap();
    let alice = workdir(&repo, 1);
    write(&root, "src/shop.ts", &SHOP.replace("return sum;", "return Math.max(sum, 0);"));
    write(&alice, "src/shop.ts", &SHOP.replace("return sum;", "return Math.abs(sum);"));
    assert_eq!(repo.integrate(LaneId(1)).unwrap().result, Outcome::Conflicts);
    repo.resolve_conflict(ConflictId(1), Decision::Lane).unwrap();

    // The main lane changes the function again before the lane asks: the
    // decision was about another text, and no longer counts.
    write(&root, "src/shop.ts", &SHOP.replace("return sum;", "return Math.max(sum, 1);"));
    let again = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(again.result, Outcome::Conflicts);
    assert_eq!((again.conflicts[0].id.as_str(), again.conflicts[0].resolution.clone()), ("conflict#1", None));
    assert!(again.conflicts[0].main.as_deref().unwrap().contains("Math.max(sum, 1)"));
    assert_eq!(repo.lane_info(LaneId(1)).unwrap().state, "blocked");

    // The lane gives in: it takes what the main lane has. Nothing to bring.
    write(&alice, "src/shop.ts", &SHOP.replace("return sum;", "return Math.max(sum, 1);"));
    let done = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(done.result, Outcome::NothingToIntegrate, "{done:?}");
    assert!(repo.conflicts(false).is_empty(), "the conflict went away with what caused it");
    assert_eq!(repo.lane_info(LaneId(1)).unwrap().state, "active");
}

#[test]
fn what_fails_the_verification_does_not_reach_the_main_lane() {
    let (_tmp, root, mut repo) = shop();
    write(
        &root,
        ".kito/config.toml",
        "[verify]\ncommands = [\"test -f src/shop.ts\", \"! grep -rq FIXME src\"]\ntimeout_secs = 30\n",
    );
    repo.reload_config().unwrap();
    repo.open_lane("alice", None, None, "work", &kitod::scope::everything()).unwrap();
    let alice = workdir(&repo, 1);
    let ops_before = repo.status().unwrap().ops;

    write(&alice, "src/shop.ts", &SHOP.replace("let sum = 0;", "let sum = 0; // FIXME"));
    write(&alice, "src/new.ts", "export const fresh = 1;\n");
    let failed = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(failed.result, Outcome::VerificationFailed, "{failed:?}");
    let report = failed.verification.unwrap();
    assert!(!report.passed);
    assert_eq!(report.failed_command.as_deref(), Some("! grep -rq FIXME src"));
    assert_eq!(report.commands.len(), 2);
    // The main lane is as it was: no op, no file, no checkpoint.
    assert_eq!(repo.status().unwrap().ops, ops_before);
    assert_eq!(read(&root, "src/shop.ts"), SHOP);
    assert!(!root.join("src/new.ts").exists());
    assert_eq!(repo.status().unwrap().checkpoints, 1);
    // The commands ran on the merged result, in a directory of their own.
    assert!(read(&root.join(".kito/verify/work"), "src/shop.ts").contains("FIXME"));
    assert!(root.join(".kito/verify/work/src/new.ts").is_file());

    // Fixed in the lane, it goes through, and the checkpoint says it was verified.
    write(&alice, "src/shop.ts", &SHOP.replace("let sum = 0;", "let sum = 0; // cents"));
    let done = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    assert!(done.verification.as_ref().unwrap().passed);
    let checkpoint = repo.store().checkpoints().next_back().unwrap().clone();
    let verification = checkpoint.verification.unwrap();
    assert!(verification.passed && verification.commands.len() == 2);
    assert!(root.join("src/new.ts").is_file());
    assert!(repo.scan().unwrap().is_empty());
}

#[test]
fn moves_renames_and_removals_arrive_as_what_they_are() {
    let (_tmp, root, mut repo) = shop();
    repo.open_lane("alice", None, None, "work", &kitod::scope::everything()).unwrap();
    repo.open_lane("bob", None, None, "work", &kitod::scope::everything()).unwrap();
    let (alice, bob) = (workdir(&repo, 1), workdir(&repo, 2));
    let discount = repo.symbols().find(Path::new("src/shop.ts"), "discount")[0];

    // Alice moves a function to a new file and renames it there; Bob edits
    // its body where it was, and removes the README.
    let start = SHOP.find("export function discount").unwrap();
    write(&alice, "src/shop.ts", &SHOP[..start]);
    write(&alice, "src/prices.ts", &SHOP[start..].replace("discount", "reduce"));
    write(&bob, "src/shop.ts", &SHOP.replace("return amount - cut;", "return Math.max(amount - cut, 0);"));
    fs::remove_file(bob.join("README.md")).unwrap();

    assert_eq!(repo.integrate(LaneId(1)).unwrap().result, Outcome::Integrated);
    let result = repo.integrate(LaneId(2)).unwrap();
    assert_eq!(result.result, Outcome::Integrated, "{result:?}");

    assert!(!read(&root, "src/shop.ts").contains("discount"));
    let prices = read(&root, "src/prices.ts");
    assert!(
        prices.contains("export function reduce(") && prices.contains("return Math.max(amount - cut, 0);"),
        "{prices}"
    );
    assert!(!root.join("README.md").exists());
    assert_eq!(repo.symbols().find(Path::new("src/prices.ts"), "reduce"), [discount]);
    assert!(repo.scan().unwrap().is_empty());

    // On the main lane the function's story reads as it happened.
    let story: Vec<String> = repo.why(Path::new("src/prices.ts"), "reduce").unwrap()[0]
        .steps
        .iter()
        .map(|s| s.what.clone())
        .collect();
    assert!(story.contains(&"moved from src/shop.ts to src/prices.ts".to_owned()), "{story:?}");
    assert!(story.contains(&"renamed `discount` → `reduce`".to_owned()), "{story:?}");
    assert!(story.iter().any(|s| s.starts_with("body changed")), "{story:?}");
    git(&root, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(
        git(&root, &["ls-files"]).lines().collect::<Vec<_>>(),
        [".gitignore", "src/prices.ts", "src/shop.ts"]
    );
}
