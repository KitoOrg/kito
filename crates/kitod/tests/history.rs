//! What was (ADR 0024): closed lanes as they ended, the checkpoints like
//! commits, and the files as they were at any of them.

use kito_core::paths::Canonical as _;
use kito_core::*;
use kito_integrate::Outcome;
use kitod::history::{CheckpointInfo, Listing};
use kitod::lanes::Stack;
use kitod::review::Change;
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
"#;

/// A shop at its first checkpoint (`ckpt#1`, "start").
fn shop() -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    drop(Repo::init(&root).unwrap());
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    write(&root, "README.md", "# Shop\n");
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn open(repo: &mut Repo, agent: &str, task: Option<&str>, intent: &str) -> (LaneId, PathBuf) {
    let info = repo.open_lane(agent, task.map(str::to_owned), None, intent, &["./".to_owned()]).unwrap();
    (info.id.parse().unwrap(), info.workdir.unwrap())
}

fn integrate(repo: &mut Repo, lane: LaneId) -> String {
    repo.scan_lane(lane).unwrap();
    let done = repo.integrate(lane).unwrap();
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    done.checkpoint.unwrap_or_default()
}

fn paths(files: &[kitod::review::FileDiff]) -> Vec<(&str, Change)> {
    files.iter().map(|file| (file.path.as_str(), file.change)).collect()
}

const TAXED: &str = "export function tax(amount: number): number {\n  return Math.round(amount * 0.22);\n}\n";

#[test]
fn a_lane_integrated_once_and_closed_shows_everything_it_did() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", None, "prices that are never negative");
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.max(price, 0);"));
    write(&work, "src/tax.ts", TAXED);
    // Said on it, and approved, before it lands.
    repo.scan_lane(lane).unwrap();
    repo.add_comment(lane, None, "is zero a price?", Some(Author::human("bob")), None, None).unwrap();
    repo.give_verdict(lane, VerdictKind::Approved, None, Some(Author::human("bob")), None).unwrap();
    let checkpoint = integrate(&mut repo, lane);
    assert_eq!(checkpoint, "ckpt#2");
    let closed = repo.close_lane(lane).unwrap();
    assert_eq!(closed.state, "closed");

    // Asked by its name, it is known; one that never was is not.
    assert_eq!(repo.known_lane("lane/1").unwrap(), lane);
    assert!(matches!(repo.known_lane("lane/9"), Err(kitod::error::Error::NoSuchLane(_))));
    let info = repo.lane_info_any(lane).unwrap();
    assert_eq!(info.state, "closed");
    assert!(info.ops > 0 && info.workdir.is_none(), "{info:?}");
    assert_eq!(info.files, ["src/shop.ts", "src/tax.ts"]);
    assert_eq!(info.integrations.len(), 1, "{info:?}");
    let once = &info.integrations[0];
    assert_eq!(
        (once.into.as_str(), once.checkpoint.as_deref(), once.by.as_deref()),
        ("main", Some("ckpt#2"), None),
        "nobody was named: who asked is not said"
    );
    let ended = info.closed.as_ref().unwrap();
    assert!(ended.at_ms.is_some_and(|at| at >= once.at_ms) && ended.by.is_none(), "{ended:?}");
    // The same in the list of every lane.
    assert_eq!(repo.lanes(true).into_iter().find(|one| one.id == "lane/1").unwrap(), info);

    // It integrated all it did: nothing is left, and its checkpoint is what it changed.
    let left = repo.lane_diff(lane, &[]).unwrap();
    assert!(left.files.is_empty(), "{left:#?}");
    let landed = repo.diff("ckpt#2", None, &[]).unwrap();
    assert_eq!((landed.from.as_deref(), landed.to.as_str()), (Some("ckpt#1"), "ckpt#2"));
    assert_eq!(paths(&landed.files), [("src/shop.ts", Change::Changed), ("src/tax.ts", Change::Added)]);
    let shop = &landed.files[0];
    assert_eq!((shop.added, shop.removed), (1, 1));
    // The functions say why, as the lane's ops did.
    // Kito's own `integrate lane/1: …` is not said beside them.
    let total = shop.symbols.iter().find(|symbol| symbol.symbol == "src/shop.ts:total").unwrap();
    assert_eq!(total.intents, ["prices that are never negative"], "{total:?}");
    let info = repo.checkpoint_whole("ckpt#2").unwrap();
    assert_eq!(
        (info.lane.as_deref(), info.why.as_deref()),
        (Some("lane/1"), Some("prices that are never negative"))
    );
    // A closed lane as a point: compared, listed, its files.
    let to_lane = repo.diff("lane/1", Some("ckpt#1"), &[]).unwrap();
    assert_eq!(paths(&to_lane.files), [("src/shop.ts", Change::Changed), ("src/tax.ts", Change::Added)]);
    let src = repo.tree_listing(Some("lane/1"), Some("src")).unwrap();
    assert_eq!((src.at.as_str(), src.entries.len()), ("lane/1", 2));
    let at_lane: Vec<String> =
        repo.files_at("lane/1", None).unwrap().into_iter().map(|file| file.path).collect();
    assert_eq!(at_lane, [".gitignore", "README.md", "src/shop.ts", "src/tax.ts"]);

    // Its operations, its files, and why its code is so.
    let log = repo.log_lane(lane, 100, None).unwrap();
    assert!(!log.is_empty());
    assert!(log.iter().all(|item| match item {
        kitod::repo::LogItem::Op { lane, .. } => lane == "lane/1",
        kitod::repo::LogItem::Checkpoint { .. } => true,
    }));
    let files: Vec<String> = repo.lane_files(lane, None).unwrap().into_iter().map(|file| file.path).collect();
    assert_eq!(files, [".gitignore", "README.md", "src/shop.ts", "src/tax.ts"]);
    let graph = repo.why_graph(lane, "src/tax.ts:tax", false).unwrap();
    assert!(serde_json::to_string(&graph).unwrap().contains("prices that are never negative"));
    assert!(repo.why_lane(lane, Path::new("src/shop.ts"), "total").is_ok());
    // What was said on it, and how its review stood, as when it closed.
    assert_eq!(repo.comments(lane, true).unwrap().len(), 1);
    assert_eq!(repo.comments(lane, false).unwrap().len(), 1);
    let review = repo.review(lane).unwrap();
    assert_eq!((review.approved_by.as_slice(), review.verdicts.len()), (&["bob".to_owned()][..], 1));
    // And nothing of it is open again.
    assert!(repo.lanes(false).is_empty());
    assert!(repo.lane_of("lane/1").is_err());
    assert!(repo.set_intent(lane, "something else entirely", None, None).is_err());
    assert!(repo.give_verdict(lane, VerdictKind::Withdrawn, None, Some(Author::human("bob")), None).is_err());
}

#[test]
fn a_lane_integrated_twice_says_both_and_each_checkpoint_is_its_part() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", None, "taxes in the shop");
    write(&work, "src/tax.ts", TAXED);
    assert_eq!(integrate(&mut repo, lane), "ckpt#2");
    repo.set_intent(lane, "one rate for every tax", None, None).unwrap();
    write(&work, "src/rates.ts", "export const RATE = 0.22;\n");
    assert_eq!(integrate(&mut repo, lane), "ckpt#3");
    repo.close_lane(lane).unwrap();
    // Each checkpoint says why with the intent of its own part, not the lane's last.
    let page = repo.checkpoints(LaneId::MAIN, 10, None, None).unwrap();
    let whys: Vec<Option<&str>> = page.checkpoints.iter().map(|ck| ck.why.as_deref()).collect();
    assert_eq!(whys, [Some("one rate for every tax"), Some("taxes in the shop"), None]);
    let last = repo.tree_listing(None, Some("src")).unwrap();
    let why =
        |name: &str| last.entries.iter().find(|entry| entry.name == name).unwrap().last.clone().unwrap().why;
    assert_eq!(
        (why("tax.ts"), why("rates.ts")),
        ("taxes in the shop".to_owned(), "one rate for every tax".to_owned())
    );

    let info = repo.lane_info_any(lane).unwrap();
    let done: Vec<Option<&str>> = info.integrations.iter().map(|one| one.checkpoint.as_deref()).collect();
    assert_eq!(done, [Some("ckpt#2"), Some("ckpt#3")]);
    assert_eq!(paths(&repo.diff("ckpt#2", None, &[]).unwrap().files), [("src/tax.ts", Change::Added)]);
    assert_eq!(paths(&repo.diff("ckpt#3", None, &[]).unwrap().files), [("src/rates.ts", Change::Added)]);
    assert_eq!(info.files, ["src/rates.ts", "src/tax.ts"]);
}

#[test]
fn a_lane_given_up_has_all_it_did_left() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "bob", None, "a discount for regulars");
    write(
        &work,
        "src/shop.ts",
        &format!("{SHOP}\nexport function discount(amount: number): number {{\n  return amount * 0.9;\n}}\n"),
    );
    repo.scan_lane(lane).unwrap();
    repo.close_lane(lane).unwrap();

    let info = repo.lane_info_any(lane).unwrap();
    assert!(info.integrations.is_empty() && info.closed.is_some(), "{info:?}");
    let left = repo.lane_diff(lane, &[]).unwrap();
    assert_eq!((left.lane.as_str(), left.onto.as_str(), left.base.as_str()), ("lane/1", "main", "ckpt#1"));
    assert_eq!(paths(&left.files), [("src/shop.ts", Change::Changed)]);
    let shop = &left.files[0];
    assert!(!shop.hunks.is_empty() && shop.added == 4, "{shop:#?}");
    let discount = shop.symbols.iter().find(|symbol| symbol.symbol == "src/shop.ts:discount").unwrap();
    assert_eq!(
        (discount.change, discount.intents.as_slice()),
        (Change::Added, &["a discount for regulars".to_owned()][..])
    );
    // Its files are what it left; the main lane never had them.
    let shop_then = repo.lane_files(lane, Some(&[PathBuf::from("src/shop.ts")])).unwrap();
    assert_ne!(
        shop_then[0].blob,
        repo.lane_files(LaneId::MAIN, Some(&[PathBuf::from("src/shop.ts")])).unwrap()[0].blob
    );
    let compared = repo.diff("lane/1", Some("main"), &[]).unwrap();
    assert_eq!(paths(&compared.files), [("src/shop.ts", Change::Changed)]);
}

#[test]
fn a_lane_that_landed_on_a_lane_says_so_without_a_checkpoint() {
    let (_tmp, _root, mut repo) = shop();
    let (api, _) = open(&mut repo, "alice", None, "the shop's api");
    let ui = repo
        .open_lane_on(
            "bob",
            None,
            None,
            "the shop's pages",
            &["./".to_owned()],
            Stack { on: Some(api), target: None },
        )
        .unwrap();
    let (ui, ui_work): (LaneId, PathBuf) = (ui.id.parse().unwrap(), ui.workdir.unwrap());
    write(&ui_work, "src/page.ts", "export const PAGE = 1;\n");
    repo.scan_lane(ui).unwrap();
    let landed = repo.integrate(ui).unwrap();
    assert_eq!(landed.landed_on.as_deref(), Some("lane/1"), "{landed:?}");
    let info = repo.lane_info_any(ui).unwrap();
    assert_eq!(info.integrations.len(), 1);
    assert_eq!(
        (info.integrations[0].into.as_str(), info.integrations[0].checkpoint.as_deref()),
        ("lane/1", None)
    );
    assert!(info.closed.is_none());
}

#[test]
fn lanes_and_checkpoints_from_before_are_read_and_nothing_is_invented() {
    let (tmp, root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", Some("PROJ-3"), "taxes in the shop");
    write(&work, "src/tax.ts", TAXED);
    assert_eq!(integrate(&mut repo, lane), "ckpt#2");
    repo.close_lane(lane).unwrap();
    // As Kito 0.40 left them: no integrations, no closing, no time on
    // the checkpoints, nothing said of which lane made one.
    let mut record = repo.store().lane_record(lane).unwrap().clone();
    (record.integrations, record.closed, record.ends_kept) = (Vec::new(), None, false);
    repo.store_mut().put_lane(&record).unwrap();
    for id in [1, 2] {
        let mut checkpoint = repo.store().checkpoint(CheckpointId(id)).unwrap().clone();
        (checkpoint.at_ms, checkpoint.integrated) = (None, None);
        repo.store_mut().put_checkpoint(&checkpoint).unwrap();
    }
    repo.close().unwrap();
    let repo = Repo::open(&root).unwrap();

    let info = repo.lane_info_any(lane).unwrap();
    assert_eq!(info.integrations.len(), 1, "worked out from the history: {info:?}");
    let once = &info.integrations[0];
    // Who asked was not kept anywhere: it is not said.
    assert_eq!(
        (once.into.as_str(), once.checkpoint.as_deref(), once.by.as_deref()),
        ("main", Some("ckpt#2"), None)
    );
    let ended = info.closed.unwrap();
    assert_eq!((ended.at_ms, ended.by), (None, None));
    // The checkpoint still says the lane it came from, read from its reason.
    let page = repo.checkpoints(LaneId::MAIN, 10, None, None).unwrap();
    let made = &page.checkpoints[0];
    assert_eq!(
        (made.id.as_str(), made.lane.as_deref(), made.agent.as_deref()),
        ("ckpt#2", Some("lane/1"), Some("alice"))
    );
    assert_eq!(made.task.as_deref(), Some("PROJ-3"));
    // Its time is its last op's.
    assert!(made.at_ms.is_some());
    drop(tmp);
}

/// The main lane changed by hand and checkpointed, `n` times.
fn history(repo: &mut Repo, root: &Path, n: usize) {
    for k in 0..n {
        write(root, &format!("notes/{k}.md"), &format!("note {k}\n"));
        repo.scan().unwrap();
        repo.checkpoint(&format!("note {k}")).unwrap();
    }
}

fn ids(page: &[CheckpointInfo]) -> Vec<&str> {
    page.iter().map(|ck| ck.id.as_str()).collect()
}

#[test]
fn checkpoints_are_listed_newest_first_a_page_at_a_time() {
    let (_tmp, root, mut repo) = shop();
    history(&mut repo, &root, 5);
    let all = repo.checkpoints(LaneId::MAIN, 50, None, None).unwrap();
    assert_eq!(ids(&all.checkpoints), ["ckpt#6", "ckpt#5", "ckpt#4", "ckpt#3", "ckpt#2", "ckpt#1"]);
    assert!(!all.more);
    let first = &all.checkpoints[5];
    assert_eq!((first.label.as_str(), first.parent.as_deref(), first.files), ("start", None, 3));
    assert_eq!(all.checkpoints[0].parent.as_deref(), Some("ckpt#5"));
    assert_eq!(
        (all.checkpoints[0].files, all.checkpoints[0].lines_added, all.checkpoints[0].by.as_str()),
        (1, 1, "ada")
    );

    // Walked a page at a time, each comes once.
    let mut seen = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let page = repo.checkpoints(LaneId::MAIN, 2, before.as_deref(), None).unwrap();
        seen.extend(page.checkpoints.iter().map(|ck| ck.id.clone()));
        before = page.checkpoints.last().map(|ck| ck.id.clone());
        if !page.more {
            break;
        }
    }
    assert_eq!(seen, ids(&all.checkpoints));
    // Those that changed a file; a directory.
    let one = repo.checkpoints(LaneId::MAIN, 50, None, Some("notes/2.md")).unwrap();
    assert_eq!(ids(&one.checkpoints), ["ckpt#4"]);
    let under = repo.checkpoints(LaneId::MAIN, 2, None, Some("notes/")).unwrap();
    assert_eq!((ids(&under.checkpoints), under.more), (vec!["ckpt#6", "ckpt#5"], true));
    // An ordinary lane has none; a checkpoint that is not one is refused.
    let (lane, _) = open(&mut repo, "alice", None, "taxes in the shop");
    assert!(repo.checkpoints(lane, 5, None, None).unwrap_err().to_string().contains("not a permanent lane"));
    assert!(repo.checkpoints(LaneId::MAIN, 5, Some("ckpt#99"), None).is_err());
}

#[test]
fn a_checkpoint_says_each_file_it_added_modified_removed_or_renamed() {
    let (_tmp, root, mut repo) = shop();
    write(&root, "src/tax.ts", TAXED);
    write(&root, "src/shop.ts", &SHOP.replace("return sum;", "return Math.round(sum);"));
    fs::remove_file(root.join("README.md")).unwrap();
    repo.scan().unwrap();
    repo.checkpoint("taxes").unwrap();
    fs::rename(root.join("src/tax.ts"), root.join("src/taxes.ts")).unwrap();
    repo.scan().unwrap();
    repo.checkpoint("a better name").unwrap();

    let taxes = repo.checkpoint_whole("ckpt#2").unwrap();
    let changed: Vec<(&str, &str, usize, usize)> = taxes
        .changed
        .as_ref()
        .unwrap()
        .iter()
        .map(|file| (file.path.as_str(), file.status.as_str(), file.lines_added, file.lines_removed))
        .collect();
    assert_eq!(
        changed,
        [("README.md", "removed", 0, 1), ("src/shop.ts", "modified", 1, 1), ("src/tax.ts", "added", 3, 0)]
    );
    assert_eq!((taxes.files, taxes.lines_added, taxes.lines_removed), (3, 4, 2));
    let renamed = repo.checkpoint_whole("ckpt#3").unwrap();
    let file = &renamed.changed.as_ref().unwrap()[0];
    assert_eq!(
        (file.path.as_str(), file.status.as_str(), file.from.as_deref()),
        ("src/taxes.ts", "renamed", Some("src/tax.ts"))
    );
    assert_eq!(renamed.files, 1);
    assert!(repo.checkpoint_whole("ckpt#9").unwrap_err().to_string().contains("no such checkpoint"));
}

#[test]
fn a_permanent_lane_has_checkpoints_of_its_own() {
    let (_tmp, _root, mut repo) = shop();
    let staging: LaneId = repo.create_lane("staging", None).unwrap().id.parse().unwrap();
    let lane = repo
        .open_lane_on(
            "alice",
            None,
            None,
            "taxes in the shop",
            &["./".to_owned()],
            Stack { on: Some(staging), target: None },
        )
        .unwrap();
    let (lane, work): (LaneId, PathBuf) = (lane.id.parse().unwrap(), lane.workdir.unwrap());
    write(&work, "src/tax.ts", TAXED);
    let made = integrate(&mut repo, lane);
    let page = repo.checkpoints(staging, 10, None, None).unwrap();
    assert_eq!(ids(&page.checkpoints), [made.as_str()]);
    let ck = &page.checkpoints[0];
    assert_eq!((ck.on.as_deref(), ck.lane.as_deref(), ck.files), (Some("staging"), Some("lane/2"), 1));
    assert_eq!(repo.lane_info_any(lane).unwrap().integrations[0].into, "staging");
}

#[test]
fn two_points_are_compared_and_the_wrong_ones_refused() {
    let (_tmp, root, mut repo) = shop();
    history(&mut repo, &root, 2);
    // Between two checkpoints, with the paths asked for.
    let both = repo.diff("ckpt#3", Some("ckpt#1"), &[]).unwrap();
    assert_eq!(paths(&both.files), [("notes/0.md", Change::Added), ("notes/1.md", Change::Added)]);
    let one = repo.diff("ckpt#3", Some("ckpt#1"), &["notes/1.md".to_owned()]).unwrap();
    assert_eq!(paths(&one.files), [("notes/1.md", Change::Added)]);
    // From the first one: everything.
    let first = repo.diff("ckpt#1", None, &[]).unwrap();
    assert_eq!((first.from, first.files.len()), (None, 3));
    // To the main lane as it is.
    write(&root, "notes/0.md", "note zero\n");
    repo.scan().unwrap();
    let now = repo.diff("main", Some("ckpt#3"), &[]).unwrap();
    assert_eq!(paths(&now.files), [("notes/0.md", Change::Changed)]);
    // Refused: the same point twice, a point that is not there, no `from` for a lane.
    assert!(repo.diff("ckpt#2", Some("ckpt#2"), &[]).unwrap_err().to_string().contains("itself"));
    assert!(repo.diff("ckpt#7", None, &[]).unwrap_err().to_string().contains("ckpt#7"));
    assert!(matches!(repo.diff("main", Some("lane/5"), &[]), Err(kitod::error::Error::NoSuchLane(_))));
    assert!(repo.diff("main", None, &[]).unwrap_err().to_string().contains("needs `from`"));
}

fn names(listing: &Listing) -> Vec<(&str, &str)> {
    listing.entries.iter().map(|entry| (entry.name.as_str(), entry.kind.as_str())).collect()
}

#[test]
fn a_directory_is_listed_as_it_was_with_the_last_change_of_each_entry() {
    let (_tmp, root, mut repo) = shop();
    history(&mut repo, &root, 2);
    fs::remove_file(root.join("README.md")).unwrap();
    write(&root, "src/readme.txt", "the shop's code\n");
    repo.scan().unwrap();
    repo.checkpoint("the readme goes with the code").unwrap();

    let now = repo.tree_listing(None, None).unwrap();
    assert_eq!((now.at.as_str(), now.path.as_str()), ("main", ""));
    assert_eq!(names(&now), [("notes", "dir"), ("src", "dir"), (".gitignore", "file")]);
    assert_eq!(now.readme, None);
    let last = |listing: &Listing, name: &str| {
        listing
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.last.clone())
            .map(|last| last.checkpoint)
    };
    // A directory: the last checkpoint that changed anything under it.
    assert_eq!(last(&now, "notes").as_deref(), Some("ckpt#3"));
    assert_eq!(last(&now, "src").as_deref(), Some("ckpt#4"));
    assert_eq!(last(&now, ".gitignore").as_deref(), Some("ckpt#1"));
    let src = repo.tree_listing(None, Some("src/")).unwrap();
    assert_eq!(names(&src), [("readme.txt", "file"), ("shop.ts", "file")]);
    assert_eq!(src.readme.as_deref(), Some("readme.txt"));
    let shop = src.entries.iter().find(|entry| entry.name == "shop.ts").unwrap();
    assert!(shop.blob.is_some() && !shop.executable);
    assert_eq!(
        shop.last.as_ref().map(|last| (last.checkpoint.as_str(), last.why.as_str())),
        Some(("ckpt#1", "start"))
    );

    // At an older checkpoint: what did not exist yet is not there, what was removed since is.
    let then = repo.tree_listing(Some("ckpt#2"), None).unwrap();
    assert_eq!(
        names(&then),
        [("notes", "dir"), ("src", "dir"), (".gitignore", "file"), ("README.md", "file")]
    );
    assert_eq!(then.readme.as_deref(), Some("README.md"));
    assert_eq!(last(&then, "notes").as_deref(), Some("ckpt#2"));
    let notes = repo.tree_listing(Some("ckpt#2"), Some("notes")).unwrap();
    assert_eq!(names(&notes), [("0.md", "file")]);
    // The whole list as it was.
    let files: Vec<String> =
        repo.files_at("ckpt#2", None).unwrap().into_iter().map(|file| file.path).collect();
    assert_eq!(repo.files_at("main", None).unwrap().len(), 5);
    assert_eq!(files, [".gitignore", "README.md", "notes/0.md", "src/shop.ts"]);
    assert!(repo.files_at("ckpt#99", None).is_err());

    // Refused: a file, a directory that is not there then.
    assert!(repo.tree_listing(None, Some("src/shop.ts")).unwrap_err().to_string().contains("is a file"));
    assert!(repo
        .tree_listing(Some("ckpt#1"), Some("notes"))
        .unwrap_err()
        .to_string()
        .contains("no such directory"));
}

#[test]
fn a_checkpoint_far_back_answers_after_many_others_were_asked() {
    let (_tmp, root, mut repo) = shop();
    history(&mut repo, &root, kitod::history::STATES_KEPT + 6);
    let last = kitod::history::STATES_KEPT + 7;
    // More states are asked for than are kept.
    for n in 3..=last {
        repo.diff(&format!("ckpt#{n}"), None, &[]).unwrap();
    }
    let tree = repo.tree_listing(Some("ckpt#2"), Some("notes")).unwrap();
    assert_eq!(names(&tree), [("0.md", "file")]);
    let diff = repo.diff("ckpt#2", None, &[]).unwrap();
    assert_eq!(paths(&diff.files), [("notes/0.md", Change::Added)]);
    assert_eq!(repo.files_at("ckpt#2", None).unwrap().len(), 4);
    // The two points the other way round: the change undone, at once.
    let back = repo.diff("ckpt#2", Some(&format!("ckpt#{last}")), &[]).unwrap();
    assert_eq!(back.files.len(), kitod::history::STATES_KEPT + 5);
    assert!(back.files.iter().all(|file| file.change == Change::Removed && !file.hunks.is_empty()));
}

#[test]
fn the_first_checkpoint_of_a_permanent_lane_is_what_the_lane_changed() {
    let (_tmp, _root, mut repo) = shop();
    let staging: LaneId = repo.create_lane("staging", None).unwrap().id.parse().unwrap();
    let lane = repo
        .open_lane_on(
            "alice",
            None,
            None,
            "taxes in the shop",
            &["./".to_owned()],
            Stack { on: Some(staging), target: None },
        )
        .unwrap();
    let (lane, work): (LaneId, PathBuf) = (lane.id.parse().unwrap(), lane.workdir.unwrap());
    write(&work, "src/tax.ts", TAXED);
    let made = integrate(&mut repo, lane);
    let info = repo.checkpoint_whole(&made).unwrap();
    let diff = repo.diff(&made, None, &[]).unwrap();
    assert_eq!(paths(&diff.files), [("src/tax.ts", Change::Added)]);
    assert_eq!(info.files, diff.files.len());
    assert_eq!(diff.from.as_deref(), Some("ckpt#1"), "it started from the main lane at ckpt#1");

    // A lane on staging: the last change of an entry is staging's checkpoint.
    let next = repo
        .open_lane_on(
            "bob",
            None,
            None,
            "rates in the shop",
            &["./".to_owned()],
            Stack { on: Some(staging), target: None },
        )
        .unwrap();
    let listed = repo.tree_listing(Some(&next.id), Some("src")).unwrap();
    let tax = listed.entries.iter().find(|entry| entry.name == "tax.ts").unwrap();
    assert_eq!(tax.last.as_ref().map(|last| last.checkpoint.as_str()), Some(made.as_str()));

    // Staging landing on the main lane: its checkpoint says it came from staging.
    let landed = repo.integrate(staging).unwrap();
    assert_eq!(landed.result, Outcome::Integrated, "{landed:?}");
    let ck = repo.checkpoint_whole(landed.checkpoint.as_deref().unwrap()).unwrap();
    assert_eq!(ck.lane.as_deref(), Some("staging"));
}

#[test]
fn a_lane_names_a_task_of_the_repository_only_when_there_is_one() {
    let (_tmp, root, mut repo) = shop();
    let lock = std::sync::Mutex::new(());
    // Tasks 1 (open) and 2 (done).
    kitod::tasks::change(&root, &lock, |all| {
        all.add("Prices in cents", "", "ada", None, None)?;
        let n = all.add("Taxes", "", "ada", None, None)?;
        all.get_mut(n)?.state = "done".into();
        Ok(())
    })
    .unwrap();
    let opened = |repo: &mut Repo, task: &str| {
        repo.open_lane("alice", Some(task.to_owned()), None, "prices in whole cents", &["./".to_owned()])
    };
    // Task 1 is there: written any way, the lane is task 1's.
    assert_eq!(opened(&mut repo, "Task #1").unwrap().task.as_deref(), Some("task#1"));
    // No task 7: somebody's own label, kept as said, as before.
    assert_eq!(opened(&mut repo, "task #7").unwrap().task.as_deref(), Some("task #7"));
    // Refused, saying how to go on: a task that is done, `task#7` written so.
    let done = opened(&mut repo, "task #2").unwrap_err().to_string();
    assert!(done.contains("task#2 is done") && done.contains("kito task reopen task#2"), "{done}");
    let none = opened(&mut repo, "task#7").unwrap_err().to_string();
    assert!(none.contains("kito task list") && none.contains("without"), "{none}");
    // An intent's task is named the same way, never refused.
    let (lane, _) = open(&mut repo, "bob", None, "rates in the shop");
    repo.set_intent(lane, "rates in whole cents", Some("TASK #1".into()), None).unwrap();
    repo.set_intent(lane, "rates rounded", Some("task #9".into()), None).unwrap();
    // A lane opened before Kito 0.41 as `task #1` is not task 1's.
    let labelled = kitod::tasks::TaskLane {
        id: "lane/9".into(),
        agent: "old".into(),
        task: Some("task #1".into()),
        state: "active".into(),
        owner: None,
    };
    let all = kitod::tasks::load(&root).unwrap();
    let mine: Vec<String> = repo.task_lanes().into_iter().map(|lane| lane.id).collect();
    assert_eq!(mine, ["lane/1", "lane/2", "lane/3"]);
    let info = kitod::tasks::info(all.get(1).unwrap(), &[labelled], false);
    assert_eq!((info.state, info.lanes.len()), ("open", 0));
}
