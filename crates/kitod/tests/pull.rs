//! Commits made outside Kito, with plain git, are brought into the main
//! lane through a lane, and the result goes back to the remote.

use kito_core::*;
use kito_integrate::Outcome;
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
    fs::read_to_string(root.join(path)).unwrap_or_default()
}

fn isolate() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let empty = std::env::temp_dir().join(format!("kito-test-config-{}", std::process::id()));
        std::env::set_var("KITO_CONFIG_DIR", &empty);
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("KITO_AUTHOR", "ada");
        std::env::remove_var("KITOD_BIN");
    });
}

/// Runs git as Grace, who does not use Kito.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Grace")
        .env("GIT_AUTHOR_EMAIL", "grace@example.com")
        .env("GIT_COMMITTER_NAME", "Grace")
        .env("GIT_COMMITTER_EMAIL", "grace@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

const SHOP: &str = "export function total(prices: number[]): number {\n  let sum = 0;\n  for (const price of prices) {\n    sum += price;\n  }\n  return sum;\n}\n\nexport function discount(amount: number, percent: number): number {\n  const cut = (amount * percent) / 100;\n  return amount - cut;\n}\n";

/// A repository kept by Kito, the remote it pushes to, and the clone of
/// somebody who works with git alone.
struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    remote: PathBuf,
    grace: PathBuf,
    repo: Repo,
}

fn world() -> World {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (root, remote, grace) = (base.join("shop"), base.join("remote.git"), base.join("grace"));
    fs::create_dir(&root).unwrap();
    drop(Repo::init(&root).unwrap());
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    write(&root, "README.md", "# Shop\n");
    repo.checkpoint("a shop").unwrap();
    git(&base, &["init", "-q", "--bare", "-b", "main", remote.to_str().unwrap()]);
    git(&root, &["push", "-q", remote.to_str().unwrap(), "main"]);
    git(&base, &["clone", "-q", remote.to_str().unwrap(), grace.to_str().unwrap()]);
    World { _tmp: tmp, root, remote, grace, repo }
}

impl World {
    /// Grace changes a file, commits and pushes.
    fn commit(&self, path: &str, content: &str, message: &str) -> String {
        write(&self.grace, path, content);
        git(&self.grace, &["add", "-A"]);
        git(&self.grace, &["commit", "-q", "-m", message]);
        git(&self.grace, &["push", "-q", "origin", "main"]);
        git(&self.grace, &["rev-parse", "HEAD"])
    }

    /// What the daemon does before it pushes: fetch, and bring in.
    fn fetch(&mut self) -> Option<kitod::pull::Pull> {
        git(&self.root, &["fetch", "-q", self.remote.to_str().unwrap(), "main"]);
        let tip = git(&self.root, &["rev-parse", "FETCH_HEAD"]);
        self.repo.pull_import(&tip).unwrap()
    }

    fn push(&self) {
        git(&self.root, &["push", "-q", self.remote.to_str().unwrap(), "main"]);
    }

    /// A lane of an agent, integrated.
    fn agent(&mut self, name: &str, intent: &str, item: &str, was: &str, is: &str) {
        let lane = self.repo.open_lane(name, None, None, intent, &[item.to_owned()]).unwrap();
        let work = lane.workdir.clone().unwrap();
        let file = item.split(':').next().unwrap();
        let text = read(&work, file);
        assert!(text.contains(was), "{was:?} is not in {file}");
        write(&work, file, &text.replace(was, is));
        let id: LaneId = lane.id.parse().unwrap();
        assert_eq!(self.repo.integrate(id).unwrap().result, Outcome::Integrated);
        self.repo.close_lane(id).unwrap();
    }
}

#[test]
fn commits_pushed_with_git_are_merged_with_what_the_lanes_did() {
    let mut world = world();
    // Grace floors the discount and adds a file, in two commits.
    let floored = SHOP.replace("(amount * percent) / 100", "Math.floor((amount * percent) / 100)");
    world.commit("src/shop.ts", &floored, "whole cents off");
    let last = world.commit("NOTES.md", "Prices are in euros.\n", "say which currency");
    // Meanwhile an agent puts the totals in cents, in the same file.
    world.agent(
        "alice",
        "prices in cents",
        "src/shop.ts:total",
        "sum += price;",
        "sum += Math.round(price * 100);",
    );
    // The remote moved on: a push as it is would be refused.
    let refused = Command::new("git")
        .current_dir(&world.root)
        .args(["push", "-q", world.remote.to_str().unwrap(), "main"])
        .output()
        .unwrap();
    assert!(!refused.status.success());

    // The two commits come into a lane, each under its author and with
    // its message as the reason.
    let pull = world.fetch().expect("there are commits to bring in");
    assert_eq!((pull.commits, pull.tip.as_str()), (2, last.as_str()));
    assert_eq!(world.repo.pull_waiting(), Some((last.clone(), pull.lane)));
    let status = world.repo.lane_info(pull.lane).unwrap();
    assert_eq!(status.agent, "git");
    assert_eq!(status.scope, ["NOTES.md", "src/shop.ts"]);

    // Integrated like any lane: merged function by function with alice's.
    let done = world.repo.integrate(pull.lane).unwrap();
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    world.repo.pull_done(pull.lane).unwrap();
    assert_eq!(world.repo.pull_waiting(), None);
    let merged = read(&world.root, "src/shop.ts");
    assert!(merged.contains("Math.round(price * 100)") && merged.contains("Math.floor("), "{merged}");
    assert_eq!(read(&world.root, "NOTES.md"), "Prices are in euros.\n");

    // In git it is a merge with Grace's last commit: the remote takes it.
    let parents = git(&world.root, &["rev-list", "--parents", "-n", "1", "main"]);
    let parents: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parents.len(), 3, "a commit and its two parents: {parents:?}");
    assert_eq!(parents[2], last);
    world.push();
    assert_eq!(git(&world.remote, &["rev-parse", "main"]), parents[0]);
    // Nothing more to bring.
    assert_eq!(world.fetch(), None);

    // Why the discount is the way it is: Grace's commit says.
    let why = world.repo.why_graph(LaneId::MAIN, "src/shop.ts:discount", false).unwrap();
    let reason = &why.reasons[0];
    assert_eq!((reason.why.as_str(), reason.by.as_str()), ("whole cents off", "Grace"));
    assert!(reason.task.as_deref().unwrap().starts_with("git "), "{reason:?}");
    let notes = world.repo.why_graph(LaneId::MAIN, "NOTES.md", false).unwrap();
    assert_eq!(notes.reasons[0].why, "say which currency");

    // Grace takes the merge and goes on from it; so does Kito.
    git(&world.grace, &["pull", "-q", "origin", "main"]);
    assert!(read(&world.grace, "src/shop.ts").contains("Math.round(price * 100)"));
    let next = world.commit("README.md", "# The shop\n", "a better title");
    world.agent("bob", "a second note", "NOTES.md", "euros", "euros, in cents");
    let pull = world.fetch().expect("one more commit");
    assert_eq!((pull.commits, pull.tip.as_str()), (1, next.as_str()));
    assert_eq!(world.repo.integrate(pull.lane).unwrap().result, Outcome::Integrated);
    world.repo.pull_done(pull.lane).unwrap();
    world.push();
    assert_eq!(read(&world.root, "README.md"), "# The shop\n");
    assert_eq!(read(&world.root, "NOTES.md"), "Prices are in euros, in cents.\n");
}

#[test]
fn commits_that_conflict_wait_in_their_lane_until_somebody_decides() {
    let mut world = world();
    world.commit(
        "src/shop.ts",
        &SHOP.replace("sum += price;", "sum += Math.ceil(price);"),
        "round prices up",
    );
    world.agent(
        "alice",
        "prices in cents",
        "src/shop.ts:total",
        "sum += price;",
        "sum += Math.round(price * 100);",
    );

    let pull = world.fetch().unwrap();
    let stuck = world.repo.integrate(pull.lane).unwrap();
    assert_eq!(stuck.result, Outcome::Conflicts, "{stuck:?}");
    assert_eq!(stuck.conflicts.len(), 1);
    // Asked again, the same lane is still the one to integrate.
    let again = world.fetch().unwrap();
    assert_eq!((again.lane, again.commits), (pull.lane, 0));

    // More commits arrive meanwhile: they join the lane.
    let more = world.commit("NOTES.md", "Rounded up.\n", "say how prices are rounded");
    let grown = world.fetch().unwrap();
    assert_eq!((grown.lane, grown.commits, grown.tip.as_str()), (pull.lane, 1, more.as_str()));

    // Decided for the main lane's side, the rest goes in.
    let conflict: ConflictId = stuck.conflicts[0].id.parse().unwrap();
    world.repo.resolve_conflict(conflict, kitod::integrate::Decision::Main).unwrap();
    let done = world.repo.integrate(pull.lane).unwrap();
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    world.repo.pull_done(pull.lane).unwrap();
    assert!(read(&world.root, "src/shop.ts").contains("Math.round(price * 100)"));
    assert_eq!(read(&world.root, "NOTES.md"), "Rounded up.\n");
    world.push();
}

#[test]
fn nothing_is_brought_in_when_there_is_nothing_or_nothing_in_common() {
    let mut world = world();
    assert_eq!(world.fetch(), None, "the remote has what was pushed");
    // A commit that is no commit.
    assert!(world.repo.pull_import("0000000000000000000000000000000000000000").is_err());
    // A history that shares nothing with this repository.
    let base = world.root.parent().unwrap().to_owned();
    let stranger = base.join("stranger");
    fs::create_dir(&stranger).unwrap();
    git(&stranger, &["init", "-q", "-b", "main"]);
    write(&stranger, "x.txt", "x\n");
    git(&stranger, &["add", "-A"]);
    git(&stranger, &["commit", "-q", "-m", "elsewhere"]);
    git(&world.root, &["fetch", "-q", stranger.to_str().unwrap(), "main"]);
    let tip = git(&world.root, &["rev-parse", "FETCH_HEAD"]);
    let error = world.repo.pull_import(&tip).unwrap_err().to_string();
    assert!(error.contains("no history in common"), "{error}");
    assert!(world.repo.lanes(false).is_empty(), "no lane is left behind");
}
