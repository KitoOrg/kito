//! Level 2 in an integration: a rename made on one side reaches the uses
//! the other side has of the symbol, or asks.

use kito_core::*;
use kito_integrate::{IntegrationResult, Outcome};
use kitod::integrate::{Decision, Preparation};
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

const AUTH: &str = "export function getUser(id: string): string {\n  return id;\n}\n\nexport function current(): string {\n  return getUser(\"me\");\n}\n";
const WEB: &str =
    "import { getUser } from \"./auth\";\n\nexport function profile(id: string): string {\n  return getUser(id);\n}\n";
const BILLING: &str =
    "import { getUser } from \"./auth\";\n\nexport function invoice(id: string): string {\n  return \"invoice for \" + getUser(id);\n}\n";

/// The language server of the tests, which is sure of everything: built as
/// an example of this crate.
fn sure_server() -> String {
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().unwrap().parent().unwrap().join("examples/fake_language_server");
    assert!(
        path.is_file(),
        "{} is not built: `cargo test -p kitod` builds it, a single `--test` does not (then: `cargo build -p kitod --example fake_language_server`)",
        path.display()
    );
    format!(
        "[semantic.servers.test]\ncommand = [{:?}]\nlanguages = [\"typescript\"]\ntimeout_secs = 20\n",
        path
    )
}

/// No language server: the syntax alone.
const NO_SERVER: &str = "[semantic]\nservers = {}\n";

/// A repository with one checkpoint and two lanes, alice's and bob's.
fn project(semantic: &str) -> (tempfile::TempDir, PathBuf, Repo, PathBuf, PathBuf) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mut repo = Repo::init(&root).unwrap();
    write(&root, ".kito/config.toml", &format!("[git]\nexport_on_checkpoint = false\n{semantic}"));
    repo.reload_config().unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/auth.ts", AUTH);
    write(&root, "src/web.ts", WEB);
    repo.checkpoint("start").unwrap();
    repo.open_lane("alice", None, None).unwrap();
    repo.open_lane("bob", None, None).unwrap();
    let alice = repo.lane_info(LaneId(1)).unwrap().workdir.unwrap();
    let bob = repo.lane_info(LaneId(2)).unwrap().workdir.unwrap();
    (tmp, root, repo, alice, bob)
}

/// Alice renames the function everywhere she knows it is used.
fn rename_everywhere(alice: &Path) {
    write(alice, "src/auth.ts", &AUTH.replace("getUser", "fetchUser"));
    write(alice, "src/web.ts", &WEB.replace("getUser", "fetchUser"));
}

fn followed(result: &IntegrationResult) -> Vec<String> {
    result
        .auto_resolutions
        .iter()
        .filter(|auto| auto.kind == "rename_propagation")
        .map(|auto| {
            format!("{} -> {}: {}", auto.symbol, auto.new_name.clone().unwrap(), auto.locations.join(", "))
        })
        .collect()
}

#[test]
fn a_rename_reaches_the_use_another_lane_added() {
    let (_tmp, root, mut repo, alice, bob) = project(&sure_server());
    rename_everywhere(&alice);
    // Bob, meanwhile: a new file that uses the function, and one more use
    // in a function alice's rename also touches.
    write(&bob, "src/billing.ts", BILLING);
    write(
        &bob,
        "src/web.ts",
        &WEB.replace("return getUser(id);", "const again = getUser(id);\n  return getUser(id) + again;"),
    );

    let first = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(first.result, Outcome::Integrated, "{first:?}");
    assert!(followed(&first).is_empty(), "{first:?}");

    // Bob's work arrives on a main lane where the function has a new name:
    // his uses take it, in the new file and in the one both changed.
    let second = repo.integrate(LaneId(2)).unwrap();
    assert_eq!(second.result, Outcome::Integrated, "{second:?}");
    assert_eq!(
        followed(&second),
        // (The import in web.ts is as alice left it: bob did not touch that line.)
        ["ts/auth.getUser#0 -> fetchUser: src/billing.ts:1, src/billing.ts:4, src/web.ts:4, src/web.ts:5"]
    );
    assert_eq!(read(&root, "src/billing.ts"), BILLING.replace("getUser", "fetchUser"));
    assert_eq!(
        read(&root, "src/web.ts"),
        WEB.replace("return getUser(id);", "const again = getUser(id);\n  return getUser(id) + again;")
            .replace("getUser", "fetchUser")
    );
    assert!(!read(&root, "src/auth.ts").contains("getUser"));
    // Bob's working copy was brought up to the result.
    assert_eq!(read(&bob, "src/billing.ts"), BILLING.replace("getUser", "fetchUser"));
    // One function, one history: created, renamed on alice's lane.
    let id = repo.symbols().find(Path::new("src/auth.ts"), "fetchUser")[0];
    assert_eq!(id, SymbolId::new(LaneId::MAIN, 0));
    assert!(repo.conflicts(true).is_empty());
    assert!(repo.store().verify().unwrap().is_ok());
}

#[test]
fn a_rename_reaches_the_uses_the_main_lane_got_meanwhile() {
    let (_tmp, root, mut repo, alice, bob) = project(&sure_server());
    // This time bob arrives first, and alice forgets a file.
    write(&bob, "src/billing.ts", BILLING);
    write(&alice, "src/auth.ts", &AUTH.replace("getUser", "fetchUser"));

    assert_eq!(repo.integrate(LaneId(2)).unwrap().result, Outcome::Integrated);
    let result = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(result.result, Outcome::Integrated, "{result:?}");
    // Her rename is carried to the use bob added and to the one she forgot.
    assert_eq!(
        followed(&result),
        ["ts/auth.getUser#0 -> fetchUser: src/billing.ts:1, src/billing.ts:4, src/web.ts:1, src/web.ts:4"]
    );
    for file in ["src/auth.ts", "src/web.ts", "src/billing.ts"] {
        let text = read(&root, file);
        assert!(text.contains("fetchUser") && !text.contains("getUser"), "{file}: {text}");
        assert_eq!(read(&alice, file), text, "the lane continues from the result");
    }
    // They are recorded as part of her integration.
    let log = repo.log(20, Some(Path::new("src/billing.ts"))).unwrap();
    assert!(format!("{log:?}").contains("alice"), "{log:?}");
}

#[test]
fn a_use_the_syntax_only_suspects_is_asked_about() {
    let (_tmp, root, mut repo, alice, bob) = project(NO_SERVER);
    rename_everywhere(&alice);
    write(&bob, "src/billing.ts", BILLING);
    assert_eq!(repo.integrate(LaneId(1)).unwrap().result, Outcome::Integrated);

    let asked = repo.integrate(LaneId(2)).unwrap();
    assert_eq!(asked.result, Outcome::Conflicts, "{asked:?}");
    assert_eq!(asked.conflicts.len(), 1);
    let conflict = &asked.conflicts[0];
    assert_eq!((conflict.kind.as_str(), conflict.symbol.as_deref()), ("semantic_ref", Some("getUser")));
    assert_eq!(conflict.base.as_deref(), Some("getUser"));
    assert_eq!(conflict.main.as_deref(), Some("renamed: getUser → fetchUser\n"));
    assert_eq!(
        conflict.lane_text.as_deref(),
        Some(
            "uses of `getUser` that may be of this symbol:\nsrc/billing.ts:1: import { getUser } from \"./auth\";\nsrc/billing.ts:4: return \"invoice for \" + getUser(id);\n"
        )
    );
    assert_eq!(repo.lane_info(LaneId(2)).unwrap().state, "blocked");
    assert!(!root.join("src/billing.ts").exists());

    // It is decided by saying what the uses are, not by giving a text.
    let id = ConflictId(1);
    let refused = repo.resolve_conflict(id, Decision::Text("fetchUser".into())).unwrap_err();
    assert!(refused.to_string().contains("rewrite"), "{refused}");
    repo.resolve_conflict(id, Decision::Rewrite).unwrap();
    let done = repo.integrate(LaneId(2)).unwrap();
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    assert_eq!(followed(&done), ["ts/auth.getUser#0 -> fetchUser: src/billing.ts:1, src/billing.ts:4"]);
    assert_eq!(read(&root, "src/billing.ts"), BILLING.replace("getUser", "fetchUser"));
    assert!(repo.conflicts(false).is_empty());
}

#[test]
fn uses_that_are_something_else_are_kept() {
    let (_tmp, root, mut repo, alice, bob) = project(NO_SERVER);
    rename_everywhere(&alice);
    // Bob's `getUser` is his own business: he says so.
    write(&bob, "src/billing.ts", BILLING);
    assert_eq!(repo.integrate(LaneId(1)).unwrap().result, Outcome::Integrated);
    assert_eq!(repo.integrate(LaneId(2)).unwrap().result, Outcome::Conflicts);
    repo.resolve_conflict(ConflictId(1), Decision::Keep).unwrap();
    let done = repo.integrate(LaneId(2)).unwrap();
    assert_eq!(done.result, Outcome::Integrated, "{done:?}");
    assert!(followed(&done).is_empty());
    assert_eq!(read(&root, "src/billing.ts"), BILLING);

    // The words `rewrite` and `keep` are for these conflicts only.
    write(&root, "src/auth.ts", &read(&root, "src/auth.ts").replace("return id;", "return id.trim();"));
    repo.scan().unwrap();
    write(
        &alice,
        "src/auth.ts",
        &read(&alice, "src/auth.ts").replace("return id;", "return id.toLowerCase();"),
    );
    let body = repo.integrate(LaneId(1)).unwrap();
    assert_eq!(body.conflicts[0].kind, "body_body", "{body:?}");
    let id: ConflictId = body.conflicts[0].id.parse().unwrap();
    assert!(repo.resolve_conflict(id, Decision::Rewrite).is_err());
    repo.resolve_conflict(id, Decision::Lane).unwrap();
    assert_eq!(repo.integrate(LaneId(1)).unwrap().result, Outcome::Integrated);
}

#[test]
fn an_explicit_rename_takes_along_the_uses_a_provider_vouches_for() {
    let (_tmp, root, mut repo, alice, _bob) = project(&sure_server());
    let done = repo.rename_symbol(LaneId(1), "src/auth.ts:getUser", "fetchUser").unwrap();
    assert_eq!((done.from.as_str(), done.to.as_str()), ("getUser", "fetchUser"));
    assert_eq!(done.references_updated, ["src/auth.ts:6", "src/web.ts:1", "src/web.ts:4"]);
    assert!(done.possible_references.is_empty(), "{done:?}");
    assert_eq!(read(&alice, "src/auth.ts"), AUTH.replace("getUser", "fetchUser"));
    assert_eq!(read(&alice, "src/web.ts"), WEB.replace("getUser", "fetchUser"));
    assert_eq!(read(&root, "src/web.ts"), WEB, "the main lane is not touched");
    // One rename in the history, not a removal and an addition.
    let story: Vec<String> = repo.why_lane(LaneId(1), Path::new("src/auth.ts"), "fetchUser").unwrap()[0]
        .steps
        .iter()
        .map(|step| step.what.clone())
        .collect();
    assert_eq!(story[1], "renamed `getUser` → `fetchUser`", "{story:?}");
    assert_eq!(repo.integrate(LaneId(1)).unwrap().result, Outcome::Integrated);
    assert_eq!(read(&root, "src/web.ts"), WEB.replace("getUser", "fetchUser"));

    // Without a provider that is sure, the uses are listed and left alone.
    let (_tmp, _root, mut repo, alice, _bob) = project(NO_SERVER);
    let done = repo.rename_symbol(LaneId(1), "src/auth.ts:getUser", "fetchUser").unwrap();
    assert!(done.references_updated.is_empty(), "{done:?}");
    assert_eq!(done.possible_references, ["src/auth.ts:6", "src/web.ts:1", "src/web.ts:4"]);
    assert_eq!(read(&alice, "src/web.ts"), WEB);
}

#[test]
fn a_merge_asks_for_the_references_it_was_not_given() {
    let (_tmp, _root, mut repo, alice, bob) = project(NO_SERVER);
    rename_everywhere(&alice);
    write(&bob, "README.md", "# Notes\n");
    // Nothing was renamed as far as bob's lane is concerned: no question.
    assert!(repo.plan_references(LaneId(2)).unwrap().is_none());
    assert!(matches!(repo.prepare_integration(LaneId(2), None).unwrap(), Preparation::Ready(_)));
    // Alice's rename has to be asked about before her lane is merged.
    assert!(matches!(repo.prepare_integration(LaneId(1), None).unwrap(), Preparation::Ask(_)));
    assert!(repo.plan_references(LaneId(1)).unwrap().is_some());
}
