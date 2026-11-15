//! Lanes, without the daemon: working copies of their own, captured apart.

use kito_core::*;
use kitod::Repo;
use std::fs;
use std::path::{Path, PathBuf};

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn kinds(ops: &[Op]) -> Vec<&'static str> {
    ops.iter().map(|o| o.kind.name()).collect()
}

/// No personal configuration and no git identity from the machine.
fn isolate_identity() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let empty = std::env::temp_dir().join(format!("kito-test-config-{}", std::process::id()));
        std::env::set_var("KITO_CONFIG_DIR", &empty);
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::remove_var("KITO_AUTHOR");
        std::env::remove_var("KITO_EMAIL");
        std::env::set_var("USER", "system-user");
    });
}

const AUTH: &str = "export function getUser(id: string): User {\n  const cached = cache.get(id);\n  if (cached) {\n    return cached;\n  }\n  const user = db.find(id);\n  cache.set(id, user);\n  return user;\n}\n";

fn repo_with_files() -> (tempfile::TempDir, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let mut repo = Repo::init(tmp.path()).unwrap();
    write(tmp.path(), ".gitignore", ".kito/\nnode_modules/\n");
    write(tmp.path(), "src/auth.ts", AUTH);
    write(tmp.path(), "README.md", "# Demo\n");
    repo.scan().unwrap();
    (tmp, repo)
}

#[test]
fn a_lane_has_a_working_copy_of_its_own_and_is_captured_apart() {
    let (tmp, mut repo) = repo_with_files();
    let root = tmp.path().canonicalize().unwrap();
    let main_ops = repo.status().unwrap().ops;

    // No checkpoint yet: the first one is made for the lane to be filed under.
    let lane =
        repo.open_lane("builder", Some("task #7".into()), None, "work", &kitod::scope::everything()).unwrap();
    assert_eq!((lane.id.as_str(), lane.agent.as_str(), lane.state.as_str()), ("lane/1", "builder", "active"));
    assert_eq!((lane.base.as_str(), lane.task.as_deref(), lane.ops), ("ckpt#1", Some("task #7"), 0));
    let workdir = lane.workdir.clone().unwrap();
    assert_eq!(workdir, root.join(".kito/lanes/1/work"));
    for file in [".gitignore", "src/auth.ts", "README.md"] {
        assert_eq!(fs::read(workdir.join(file)).unwrap(), fs::read(root.join(file)).unwrap(), "{file}");
    }
    assert!(!workdir.join(".kito").exists(), "the store is not part of a working copy");
    let id = LaneId(1);
    // Fresh as it is, the working copy needs no capture.
    assert!(repo.scan_lane(id).unwrap().is_empty());

    // The agent saves in its working copy: ops of the lane, signed by it,
    // with symbols born there.
    write(&workdir, "src/auth.ts", &AUTH.replace("getUser", "fetchUser"));
    write(
        &workdir,
        "src/billing.ts",
        "export function invoice(id: string): string {\n  return `invoice ${id}`;\n}\n",
    );
    write(&workdir, "node_modules/x/index.js", "ignored here as in the main directory\n");
    let ops = repo.scan_lane(id).unwrap();
    assert_eq!(kinds(&ops), ["FileAdd", "TextEdit", "AddSymbol", "Rename"]);
    assert!(ops.iter().all(|op| op.lane == id && op.author == Author::agent("builder")));
    let OpKind::AddSymbol { sym, .. } = &ops[2].kind else { panic!() };
    assert_eq!((sym.id.origin(), sym.name.as_str()), (id, "invoice"));
    // The first op of a lane hangs from the main lane's op it started at.
    assert_eq!(ops[0].parents.as_slice(), [repo.head().unwrap()]);
    assert_eq!(ops[1].parents.as_slice(), [ops[0].id]);

    // The main lane saw nothing of it, and what it does stays out of the lane.
    assert_eq!(repo.status().unwrap().ops, main_ops);
    assert!(repo.scan().unwrap().is_empty());
    assert_eq!(fs::read_to_string(root.join("src/auth.ts")).unwrap(), AUTH);
    write(&root, "README.md", "# Demo, edited on the main lane\n");
    assert_eq!(kinds(&repo.scan().unwrap()), ["TextEdit"]);
    assert!(repo.scan_lane(id).unwrap().is_empty());
    assert_eq!(fs::read_to_string(workdir.join("README.md")).unwrap(), "# Demo\n");

    let info = repo.lane_info(id).unwrap();
    assert_eq!(
        (info.ops, info.files.clone()),
        (4, vec!["src/auth.ts".to_owned(), "src/billing.ts".to_owned()])
    );
    assert_eq!(repo.lanes(false), [info]);
    assert_eq!(repo.lane_of("lane/1").unwrap(), id);
    assert_eq!(repo.lane_of("main").unwrap(), LaneId::MAIN);
    assert!(repo.lane_of("lane/9").unwrap_err().to_string().contains("no such open lane"));
    assert!(repo.store().verify().unwrap().is_ok());
}

#[test]
fn lanes_survive_a_restart_and_a_lost_working_copy() {
    let (tmp, mut repo) = repo_with_files();
    let root = tmp.path().canonicalize().unwrap();
    let first = repo.open_lane("one", None, None, "work", &kitod::scope::everything()).unwrap();
    let second = repo.open_lane("two", None, None, "work", &kitod::scope::everything()).unwrap();
    let (one, two) = (LaneId(1), LaneId(2));
    assert_eq!(second.id, "lane/2");
    write(&first.workdir.clone().unwrap(), "src/auth.ts", &AUTH.replace("getUser", "fetchUser"));
    write(&second.workdir.clone().unwrap(), "notes.md", "from the second lane\n");
    repo.scan_lane(one).unwrap();
    repo.scan_lane(two).unwrap();
    let before = [repo.lane_info(one).unwrap(), repo.lane_info(two).unwrap()];
    repo.close().unwrap();

    // From the saved state.
    let mut repo = Repo::open(&root).unwrap();
    assert_eq!(repo.lanes(false), before);
    assert!(repo.scan_lane(one).unwrap().is_empty() && repo.scan_lane(two).unwrap().is_empty());
    // Saves made while nothing was running are found.
    write(&root.join(".kito/lanes/2/work"), "notes.md", "from the second lane, edited offline\n");
    repo.close().unwrap();
    let mut repo = Repo::open(&root).unwrap();
    assert_eq!(kinds(&repo.scan_lane(two).unwrap()), ["TextEdit"]);
    let after_edit = repo.lane_info(two).unwrap();
    repo.close().unwrap();

    // From the log alone, and with a working copy that was deleted: it is
    // made again as the lane had it, not taken for a lane that removed
    // everything.
    fs::remove_file(root.join(".kito/lanes/1/state")).unwrap();
    fs::remove_file(root.join(".kito/lanes/2/state")).unwrap();
    fs::remove_dir_all(root.join(".kito/lanes/2/work")).unwrap();
    let mut repo = Repo::open(&root).unwrap();
    assert_eq!(repo.lanes(false), [before[0].clone(), after_edit]);
    assert!(repo.scan_lane(one).unwrap().is_empty() && repo.scan_lane(two).unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(root.join(".kito/lanes/2/work/notes.md")).unwrap(),
        "from the second lane, edited offline\n"
    );
    assert_eq!(
        fs::read_to_string(root.join(".kito/lanes/1/work/src/auth.ts")).unwrap(),
        AUTH.replace("getUser", "fetchUser")
    );
    // New symbols of a lane continue its own numbering.
    write(
        &root.join(".kito/lanes/1/work"),
        "src/extra.ts",
        "export function extra(): number {\n  return 1;\n}\n",
    );
    let ops = repo.scan_lane(one).unwrap();
    let OpKind::AddSymbol { sym, .. } = &ops[1].kind else { panic!("{:?}", kinds(&ops)) };
    assert_eq!(sym.id, SymbolId::new(one, 0));
}

#[test]
fn intents_closing_and_bases() {
    let (tmp, mut repo) = repo_with_files();
    let root = tmp.path().canonicalize().unwrap();
    repo.checkpoint("first").unwrap();
    write(&root, "README.md", "# Demo, second version\n");
    repo.checkpoint("second").unwrap();
    write(&root, "README.md", "# Demo, not yet in a checkpoint\n");

    // By default a lane starts from what the main lane is now; it can also
    // start from an earlier checkpoint.
    let now = repo.open_lane("now", None, None, "work", &kitod::scope::everything()).unwrap();
    let then = repo.open_lane("then", None, Some("ckpt#1"), "work", &kitod::scope::everything()).unwrap();
    assert_eq!((now.base.as_str(), then.base.as_str()), ("ckpt#2", "ckpt#1"));
    let read = |lane: &kitod::lanes::LaneInfo| {
        fs::read_to_string(lane.workdir.clone().unwrap().join("README.md")).unwrap()
    };
    assert_eq!(read(&now), "# Demo, not yet in a checkpoint\n");
    assert_eq!(read(&then), "# Demo\n");
    assert!(repo
        .open_lane("lost", None, Some("ckpt#9"), "work", &kitod::scope::everything())
        .unwrap_err()
        .to_string()
        .contains("does not exist"));
    assert!(repo.open_lane("  ", None, None, "work", &kitod::scope::everything()).is_err());

    // The "why" of what follows.
    let lane = LaneId(1);
    let workdir = now.workdir.clone().unwrap();
    write(&workdir, "a.txt", "before the intent\n");
    let intent = repo
        .set_intent(lane, "add a cache for user requests", Some("task #55".into()), Some("the full prompt"))
        .unwrap();
    write(&workdir, "b.txt", "after the intent\n");
    repo.scan_lane(lane).unwrap();
    let ops = repo.store().lane_ops(lane, 0, 10).unwrap();
    // What was saved before is under the intent the lane was opened with.
    let whys: Vec<Option<String>> = ops.iter().map(|op| op.intent.map(|i| i.to_string())).collect();
    assert!(whys[0].is_some() && whys[0] != whys[1], "{whys:?}");
    assert_eq!(whys[1], Some(intent));
    let recorded = repo.store().intent(&ops[1].intent.unwrap()).unwrap();
    assert_eq!(
        (recorded.summary.as_str(), recorded.task.as_deref()),
        ("add a cache for user requests", Some("task #55"))
    );
    assert_eq!(repo.store().get_blob(&recorded.prompt.unwrap()).unwrap().unwrap(), b"the full prompt");
    assert_eq!(repo.lane_info(lane).unwrap().intent.as_deref(), Some("add a cache for user requests"));
    assert!(repo.set_intent(lane, " ", None, None).is_err());

    // Closing records what was not yet captured, then the working copy goes.
    write(&workdir, "c.txt", "saved right before closing\n");
    let closed = repo.close_lane(lane).unwrap();
    assert_eq!((closed.state.as_str(), closed.ops, closed.workdir), ("closed", 3, None));
    assert!(!root.join(".kito/lanes/1").exists());
    assert_eq!(repo.store().lane_len(lane).unwrap(), 3);
    assert!(repo.scan_lane(lane).unwrap_err().to_string().contains("no such open lane"));
    assert_eq!(repo.lanes(false).iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["lane/2"]);
    assert_eq!(
        repo.lanes(true).iter().map(|l| (l.id.as_str(), l.state.as_str())).collect::<Vec<_>>(),
        [("lane/1", "closed"), ("lane/2", "active")]
    );
    assert!(repo.close_lane(LaneId::MAIN).is_err());

    // A closed lane stays closed, and its number is not given again.
    repo.close().unwrap();
    let mut repo = Repo::open(&root).unwrap();
    assert_eq!(repo.lanes(false).len(), 1);
    assert_eq!(repo.open_lane("next", None, None, "work", &kitod::scope::everything()).unwrap().id, "lane/3");
    let _ = PathBuf::new();
}

#[test]
fn shared_paths_are_linked_not_copied() {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mut repo = Repo::init(&root).unwrap();
    write(&root, ".gitignore", ".kito/\nnode_modules/\n");
    write(&root, "src/app.ts", "export const app = 1;\n");
    write(&root, "node_modules/lib/index.js", "module.exports = 1;\n");
    write(&root, ".kito/config.toml", "[lanes]\nshare = [\"node_modules\"]\n");
    repo.reload_config().unwrap();
    repo.scan().unwrap();

    let lane = repo.open_lane("builder", None, None, "work", &kitod::scope::everything()).unwrap();
    let workdir = lane.workdir.unwrap();
    let link = workdir.join("node_modules");
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(link.join("lib/index.js")).unwrap(), "module.exports = 1;\n");
    // The link is not something the lane did.
    assert!(repo.scan_lane(LaneId(1)).unwrap().is_empty());
}
