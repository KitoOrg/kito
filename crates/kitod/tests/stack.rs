//! Lanes on lanes (ADR 0001): a lane opened on top of another that has not
//! landed, landing on it or, once it has landed, further down.

use kito_core::LaneId;
use kito_integrate::Outcome;
use kitod::lanes::{LaneInfo, Stack};
use kitod::Repo;
use std::fs;
use std::path::{Path, PathBuf};

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

const API: &str = "export function getUser(id: string): string {\n  return id;\n}\n";
const UI: &str = "export function page(): string {\n  return \"home\";\n}\n";

/// A repository with an API and a page, and one checkpoint.
fn app() -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mut repo = Repo::init(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/api.ts", API);
    write(&root, "src/ui.ts", UI);
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn scope(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn id(info: &LaneInfo) -> LaneId {
    info.id.parse().unwrap()
}

fn open(repo: &mut Repo, agent: &str, items: &[&str], stack: Stack) -> (LaneId, PathBuf) {
    let info =
        repo.open_lane_on(agent, None, None, &format!("{agent}'s work"), &scope(items), stack).unwrap();
    (id(&info), info.workdir.unwrap())
}

fn on(lane: LaneId) -> Stack {
    Stack { on: Some(lane), target: None }
}

const API_V2: &str =
    "export function getUser(id: string): string {\n  return id.trim();\n}\n\nexport function \
                      getTeam(id: string): string {\n  return \"team of \" + id;\n}\n";
const UI_V2: &str =
    "import { getTeam } from \"./api\";\n\nexport function page(): string {\n  return getTeam(\"me\");\n}\n";

#[test]
fn a_lane_starts_on_work_that_has_not_landed_and_lands_on_it() {
    let (_tmp, root, mut repo) = app();
    let (api, api_work) = open(&mut repo, "alice", &["src/api.ts"], Stack::default());
    write(&api_work, "src/api.ts", API_V2);

    // Bob builds the page on alice's API before it lands.
    let (ui, ui_work) = open(&mut repo, "bob", &["src/ui.ts"], on(api));
    assert_eq!(read(&ui_work, "src/api.ts"), API_V2, "the lane starts from its parent as it is");
    let info = repo.lane_info(ui).unwrap();
    assert_eq!((info.parent.as_deref(), info.target.as_deref()), (Some("lane/1"), Some("lane/1")));
    write(&ui_work, "src/ui.ts", UI_V2);

    // It lands on its parent: no checkpoint, the main lane does not move.
    let checkpoints = repo.log(100, None).unwrap().len();
    let landed = repo.integrate(ui).unwrap();
    assert_eq!(landed.result, Outcome::Integrated, "{landed:?}");
    assert_eq!(landed.landed_on.as_deref(), Some("lane/1"));
    assert!(landed.checkpoint.is_none());
    assert_eq!(repo.log(100, None).unwrap().len(), checkpoints);
    assert_eq!(read(&api_work, "src/ui.ts"), UI_V2, "the parent's working copy has it");
    assert_eq!(read(&root, "src/ui.ts"), UI, "the main directory does not");
    // The parent brings it further: what landed on it is in its scope.
    assert!(repo.lane_info(api).unwrap().scope.contains(&"src/ui.ts".to_owned()));

    let both = repo.integrate(api).unwrap();
    assert_eq!(both.result, Outcome::Integrated, "{both:?}");
    assert!(both.checkpoint.is_some());
    assert_eq!((read(&root, "src/api.ts"), read(&root, "src/ui.ts")), (API_V2.to_owned(), UI_V2.to_owned()));
}

#[test]
fn a_lane_that_targets_the_main_lane_waits_for_its_parent() {
    let (_tmp, root, mut repo) = app();
    let (api, api_work) = open(&mut repo, "alice", &["src/api.ts"], Stack::default());
    write(&api_work, "src/api.ts", API_V2);
    let (ui, ui_work) =
        open(&mut repo, "bob", &["src/ui.ts"], Stack { on: Some(api), target: Some(LaneId::MAIN) });
    assert_eq!(repo.lane_info(ui).unwrap().target.as_deref(), Some("main"));
    write(&ui_work, "src/ui.ts", UI_V2);

    // Landing now would bring alice's API along unasked.
    let early = repo.integrate(ui).unwrap();
    assert_eq!(early.result, Outcome::Failed, "{early:?}");
    let said = early.message.unwrap();
    assert!(said.contains("built on lane/1") && said.contains("integrate lane/1 first"), "{said}");
    assert_eq!(read(&root, "src/ui.ts"), UI);

    // Once it has landed, bob's lane brings only what bob did, even though
    // the main lane changed meanwhile.
    assert_eq!(repo.integrate(api).unwrap().result, Outcome::Integrated);
    write(&root, "README.md", "# App\n");
    repo.checkpoint("readme").unwrap();
    let landed = repo.integrate(ui).unwrap();
    assert_eq!(landed.result, Outcome::Integrated, "{landed:?}");
    assert!(landed.landed_on.is_none() && landed.checkpoint.is_some());
    assert_eq!(read(&root, "src/ui.ts"), UI_V2);
    assert_eq!(read(&root, "src/api.ts"), API_V2);
    assert_eq!(read(&root, "README.md"), "# App\n");
    // Brought up to the main lane, it is a lane of the main lane now.
    let info = repo.lane_info(ui).unwrap();
    assert_eq!((info.parent, info.target), (None, None));
}

#[test]
fn a_parent_closes_only_once_what_its_children_were_built_on_has_landed() {
    let (_tmp, root, mut repo) = app();
    let (api, api_work) = open(&mut repo, "alice", &["src/api.ts"], Stack::default());
    write(&api_work, "src/api.ts", API_V2);
    let (ui, ui_work) = open(&mut repo, "bob", &["src/ui.ts"], on(api));
    write(&ui_work, "src/ui.ts", UI_V2);

    let refused = repo.close_lane(api).unwrap_err().to_string();
    assert!(refused.contains("lane/2 is built on work of lane/1"), "{refused}");
    assert!(repo.lane_info(api).is_ok(), "nothing was closed");

    assert_eq!(repo.integrate(api).unwrap().result, Outcome::Integrated);
    repo.close_lane(api).unwrap();
    // The child is moved where its parent's work went.
    let info = repo.lane_info(ui).unwrap();
    assert_eq!((info.parent, info.target), (None, None));
    let landed = repo.integrate(ui).unwrap();
    assert_eq!(landed.result, Outcome::Integrated, "{landed:?}");
    assert_eq!((read(&root, "src/api.ts"), read(&root, "src/ui.ts")), (API_V2.to_owned(), UI_V2.to_owned()));
}

#[test]
fn three_lanes_deep_land_one_into_the_other() {
    let (_tmp, root, mut repo) = app();
    let (feature, feature_work) = open(&mut repo, "lead", &["src/"], Stack::default());
    write(&feature_work, "src/api.ts", API_V2);
    let (page, page_work) = open(&mut repo, "bob", &["src/ui.ts"], on(feature));
    let (tests, tests_work) = open(&mut repo, "carol", &["src/ui.test.ts"], on(page));
    assert_eq!(read(&tests_work, "src/api.ts"), API_V2);
    write(&page_work, "src/ui.ts", UI_V2);
    write(&tests_work, "src/ui.test.ts", "export const cases = 1;\n");

    // The deepest lane cannot skip a level that has not landed.
    let skip = repo.open_lane_on(
        "dan",
        None,
        None,
        "x",
        &scope(&["x.ts"]),
        Stack { on: Some(tests), target: Some(LaneId(99)) },
    );
    assert!(skip.unwrap_err().to_string().contains("not below the lane"));

    assert_eq!(repo.integrate(tests).unwrap().landed_on.as_deref(), Some("lane/2"));
    assert_eq!(read(&page_work, "src/ui.test.ts"), "export const cases = 1;\n");
    assert_eq!(repo.integrate(page).unwrap().landed_on.as_deref(), Some("lane/1"));
    let done = repo.integrate(feature).unwrap();
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    assert_eq!(read(&root, "src/ui.test.ts"), "export const cases = 1;\n");
    assert_eq!(read(&root, "src/ui.ts"), UI_V2);
    assert_eq!(read(&root, "src/api.ts"), API_V2);
}

#[test]
fn stacked_lanes_are_rebuilt_from_the_log_and_mark_the_store() {
    let (_tmp, root, mut repo) = app();
    let format = || fs::read_to_string(root.join(".kito/format")).unwrap();
    assert_eq!(format(), "kito-store 2\n");
    let (api, api_work) = open(&mut repo, "alice", &["src/api.ts"], Stack::default());
    assert_eq!(format(), "kito-store 2\n", "a lane of the main lane changes nothing");
    write(&api_work, "src/api.ts", API_V2);
    let (ui, ui_work) = open(&mut repo, "bob", &["src/ui.ts"], on(api));
    assert_eq!(format(), "kito-store 3\n");
    write(&ui_work, "src/ui.ts", UI_V2);
    repo.scan_lane(ui).unwrap();
    drop(repo);

    // Without the saved states, from the log alone.
    for lane in [api, ui] {
        let _ = fs::remove_file(root.join(format!(".kito/lanes/{:x}/state", lane.0)));
    }
    let mut repo = Repo::open(&root).unwrap();
    let info = repo.lane_info(ui).unwrap();
    assert_eq!(info.parent.as_deref(), Some("lane/1"));
    assert_eq!(info.files, ["src/ui.ts"], "only what bob did differs from where bob started");
    let landed = repo.integrate(ui).unwrap();
    assert_eq!(landed.landed_on.as_deref(), Some("lane/1"), "{landed:?}");
    assert_eq!(read(&api_work, "src/ui.ts"), UI_V2);
}

#[test]
fn a_stacked_lane_is_told_why_its_parent_wrote_what_it_builds_on() {
    let (_tmp, _root, mut repo) = app();
    let (api, api_work) = open(&mut repo, "alice", &["src/api.ts"], Stack::default());
    write(&api_work, "src/api.ts", API_V2);
    repo.scan_lane(api).unwrap();
    let (ui, _) = open(&mut repo, "bob", &["src/ui.ts"], on(api));
    let answer = repo.why_graph(ui, "src/api.ts:getTeam", false).unwrap();
    let reasons: Vec<(String, Option<String>)> =
        answer.reasons.iter().map(|reason| (reason.why.clone(), reason.lane.clone())).collect();
    assert_eq!(reasons.first(), Some(&("alice's work".to_owned(), Some("lane/1".to_owned()))), "{reasons:?}");
}

#[test]
fn lines_that_came_through_a_lane_keep_the_reason_they_were_written_for() {
    let (_tmp, _root, mut repo) = app();
    let (api, api_work) = open(&mut repo, "alice", &["src/api.ts"], Stack::default());
    write(&api_work, "src/api.ts", API_V2);
    let (ui, ui_work) = open(&mut repo, "bob", &["src/ui.ts"], on(api));
    write(&ui_work, "src/ui.ts", UI_V2);
    assert!(repo.integrate(ui).unwrap().landed_on.is_some());
    assert_eq!(repo.integrate(api).unwrap().result, Outcome::Integrated);

    // On the main lane, bob's lines are bob's, for bob's reason.
    let answer = repo.why_graph(LaneId::MAIN, "src/ui.ts:page", false).unwrap();
    let changed = 1 + UI_V2.lines().position(|line| line.contains("getTeam(\"me\")")).unwrap() as u32;
    let line = answer.lines.iter().find(|run| run.from <= changed && changed <= run.to).unwrap();
    let reason = &answer.reasons[line.reason.unwrap()];
    assert_eq!((reason.why.as_str(), reason.by.as_str()), ("bob's work", "bob"), "{answer:?}");
}

#[test]
fn a_stacked_lane_is_asked_for_properly() {
    let (_tmp, _root, mut repo) = app();
    let (api, _) = open(&mut repo, "alice", &["src/api.ts"], Stack::default());
    let mut error = |stack: Stack, base: Option<&str>| {
        repo.open_lane_on("bob", None, base, "why", &scope(&["src/ui.ts"]), stack).unwrap_err().to_string()
    };
    assert!(error(on(LaneId(42)), None).contains("not an open lane"));
    assert!(error(on(api), Some("checkpoint#1")).contains("no checkpoint can be given"));
    assert!(error(Stack { on: None, target: Some(api) }, None).contains("not below the lane"));
}
