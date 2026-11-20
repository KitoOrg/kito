//! Permanent lanes (ADR 0001): a `staging` next to `main`, with
//! checkpoints of its own, its own git branch and its own checks.

use kito_core::LaneId;
use kito_integrate::Outcome;
use kitod::lanes::Stack;
use kitod::Repo;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository with two functions and one checkpoint; `config` is added
/// to its configuration.
fn app(config: &str) -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    drop(Repo::init(&root).unwrap());
    if !config.is_empty() {
        let path = root.join(".kito/config.toml");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("{text}\n{config}")).unwrap();
    }
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/api.py", "def get_user(user_id):\n    return {\"id\": user_id}\n");
    write(&root, "src/page.py", "def render(user_id):\n    return \"user \" + str(user_id)\n");
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn scope(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn create_staging(repo: &mut Repo) -> LaneId {
    repo.create_lane("staging", None).unwrap().id.parse().unwrap()
}

fn open_on(repo: &mut Repo, agent: &str, items: &[&str], on: LaneId) -> (LaneId, PathBuf) {
    let stack = Stack { on: Some(on), target: None };
    let info =
        repo.open_lane_on(agent, None, None, &format!("{agent}'s work"), &scope(items), stack).unwrap();
    (info.id.parse().unwrap(), info.workdir.unwrap())
}

const API_V2: &str = "def get_user(user_id):\n    return {\"id\": user_id, \"team\": \"core\"}\n";
const PAGE_V2: &str = "def render(user_id):\n    return \"user \" + str(user_id) + \" of core\"\n";

#[test]
fn a_permanent_lane_is_named_and_never_closed() {
    let (_tmp, _root, mut repo) = app("");
    for wrong in ["main", "", "Staging", "lane7", "-x", "a b"] {
        assert!(repo.create_lane(wrong, None).is_err(), "{wrong:?}");
    }
    let staging = repo.create_lane("staging", None).unwrap();
    assert_eq!(staging.name.as_deref(), Some("staging"));
    assert!(repo.create_lane("staging", None).unwrap_err().to_string().contains("already"));
    let id = repo.lane_of("staging").unwrap();
    assert_eq!(id.to_string(), staging.id);
    assert!(repo.close_lane(id).unwrap_err().to_string().contains("permanent lane"));
    assert!(repo.lane_info(id).unwrap().scope.is_empty());
}

#[test]
fn work_lands_on_staging_under_a_checkpoint_of_its_own_and_branch() {
    let (_tmp, root, mut repo) = app("");
    let main_commit = git(&root, &["rev-parse", "main"]);
    let staging = create_staging(&mut repo);
    let (lane, work) = open_on(&mut repo, "alice", &["src/api.py"], staging);
    write(&work, "src/api.py", API_V2);

    let landed = repo.integrate(lane).unwrap();
    assert_eq!(landed.result, Outcome::Integrated, "{landed:?}");
    assert_eq!(landed.landed_on.as_deref(), Some("staging"));
    assert!(landed.checkpoint.is_some() && landed.git_commit.is_some(), "{landed:?}");
    // The main lane did not move; staging's branch comes after it.
    assert!(!read(&root, "src/api.py").contains("team"));
    assert_eq!(git(&root, &["rev-parse", "main"]), main_commit);
    assert_eq!(git(&root, &["rev-parse", "staging^"]), main_commit);
    assert_eq!(git(&root, &["show", "staging:src/api.py"]), API_V2.trim_end());
    assert!(git(&root, &["log", "-1", "--format=%s", "staging"]).contains("alice's work"));
    assert_eq!(
        repo.log(100, None).unwrap().iter().filter(|item| format!("{item:?}").contains("Checkpoint")).count(),
        1
    );
}

#[test]
fn staging_has_its_own_checks() {
    let (_tmp, _root, mut repo) = app("[lanes.staging]\nverify = [\"! grep -rq BROKEN src\"]\n");
    let staging = create_staging(&mut repo);
    let (lane, work) = open_on(&mut repo, "alice", &["src/api.py"], staging);
    write(&work, "src/api.py", "BROKEN\n");
    let failed = repo.integrate(lane).unwrap();
    assert_eq!(failed.result, Outcome::VerificationFailed, "{failed:?}");
    write(&work, "src/api.py", API_V2);
    let landed = repo.integrate(lane).unwrap();
    assert_eq!(landed.result, Outcome::Integrated, "{landed:?}");
    assert!(landed.verification.unwrap().passed);
}

#[test]
fn staging_lands_on_main_as_a_merge_and_stays() {
    let (_tmp, root, mut repo) = app("");
    let staging = create_staging(&mut repo);
    let (api, api_work) = open_on(&mut repo, "alice", &["src/api.py"], staging);
    write(&api_work, "src/api.py", API_V2);
    assert_eq!(repo.integrate(api).unwrap().result, Outcome::Integrated);
    let (page, page_work) = open_on(&mut repo, "bob", &["src/page.py"], staging);
    write(&page_work, "src/page.py", PAGE_V2);
    assert_eq!(repo.integrate(page).unwrap().result, Outcome::Integrated);
    let staging_commit = git(&root, &["rev-parse", "staging"]);

    // Somebody else lands on main meanwhile.
    let other = repo.open_lane("carol", None, None, "a readme", &scope(&["README.md"])).unwrap();
    write(other.workdir.as_ref().unwrap(), "README.md", "# App\n");
    assert_eq!(repo.integrate(other.id.parse().unwrap()).unwrap().result, Outcome::Integrated);

    let promoted = repo.integrate(staging).unwrap();
    assert_eq!(promoted.result, Outcome::Integrated, "{promoted:?}");
    assert!(promoted.landed_on.is_none());
    assert_eq!(
        (read(&root, "src/api.py"), read(&root, "src/page.py")),
        (API_V2.to_owned(), PAGE_V2.to_owned())
    );
    assert_eq!(read(&root, "README.md"), "# App\n");
    // A merge of main and staging's branch.
    let parents = git(&root, &["log", "-1", "--format=%P", "main"]);
    assert!(parents.split(' ').any(|parent| parent == staging_commit), "{parents} / {staging_commit}");
    // Staging stays, brought up to main.
    let info = repo.lane_info(staging).unwrap();
    assert_eq!((info.name.as_deref(), info.state.as_str()), (Some("staging"), "active"));
    assert_eq!(read(Path::new(info.workdir.as_ref().unwrap()), "README.md"), "# App\n");

    // The next work on staging comes after main's merge on its branch.
    let (next, next_work) = open_on(&mut repo, "dan", &["src/page.py"], staging);
    write(&next_work, "src/page.py", "def render(user_id):\n    return \"hello\"\n");
    assert_eq!(repo.integrate(next).unwrap().result, Outcome::Integrated);
    let parents = git(&root, &["log", "-1", "--format=%P", "staging"]);
    let main_now = git(&root, &["rev-parse", "main"]);
    assert!(parents.split(' ').any(|parent| parent == main_now), "{parents} / {main_now}");
}

#[test]
fn staging_catches_up_with_what_landed_on_main_directly() {
    let (_tmp, root, mut repo) = app("");
    let staging = create_staging(&mut repo);
    let (api, api_work) = open_on(&mut repo, "alice", &["src/api.py"], staging);
    write(&api_work, "src/api.py", API_V2);
    assert_eq!(repo.integrate(api).unwrap().result, Outcome::Integrated);

    // A fix lands on main without going through staging.
    let fix = repo.open_lane("carol", None, None, "a fix", &scope(&["src/page.py"])).unwrap();
    write(fix.workdir.as_ref().unwrap(), "src/page.py", PAGE_V2);
    assert_eq!(repo.integrate(fix.id.parse().unwrap()).unwrap().result, Outcome::Integrated);

    let written = repo.catch_up(staging).unwrap();
    assert_eq!(written, ["src/page.py"]);
    let staging_dir = repo.lane_info(staging).unwrap().workdir.unwrap();
    assert_eq!(
        (read(&staging_dir, "src/page.py"), read(&staging_dir, "src/api.py")),
        (PAGE_V2.to_owned(), API_V2.to_owned())
    );
    assert!(repo.catch_up(staging).unwrap().is_empty(), "nothing more to bring");

    // Landing on main afterwards: the fix is on both sides and merges as one.
    let promoted = repo.integrate(staging).unwrap();
    assert_eq!(promoted.result, Outcome::Integrated, "{promoted:?}");
    assert_eq!(
        (read(&root, "src/page.py"), read(&root, "src/api.py")),
        (PAGE_V2.to_owned(), API_V2.to_owned())
    );

    // The same code changed on both: nothing is brought in.
    let (api2, api2_work) = open_on(&mut repo, "alice", &["src/api.py"], staging);
    write(&api2_work, "src/api.py", "def get_user(user_id):\n    return None\n");
    assert_eq!(repo.integrate(api2).unwrap().result, Outcome::Integrated);
    let other = repo.open_lane("erin", None, None, "other", &scope(&["src/api.py"])).unwrap();
    write(other.workdir.as_ref().unwrap(), "src/api.py", "def get_user(user_id):\n    return {}\n");
    assert_eq!(repo.integrate(other.id.parse().unwrap()).unwrap().result, Outcome::Integrated);
    let refused = repo.catch_up(staging).unwrap_err().to_string();
    assert!(refused.contains("changed the same code"), "{refused}");
}

#[test]
fn only_those_named_land_on_a_permanent_lane_that_says_so() {
    let (_tmp, _root, mut repo) = app("[lanes.staging]\nintegrate = [\"ci\"]\n");
    let staging = create_staging(&mut repo);
    assert!(repo.may_land_on(staging, Some("ci")).is_ok());
    assert!(repo.may_land_on(staging, None).is_ok(), "on the machine of the repository");
    let refused = repo.may_land_on(staging, Some("bob")).unwrap_err().to_string();
    assert!(refused.contains("only by ci"), "{refused}");
    assert!(repo.may_land_on(LaneId::MAIN, Some("bob")).is_ok());
}
