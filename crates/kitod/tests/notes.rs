//! Notes on code (ADR 0003): left on a function, a file or a directory,
//! told with the why of that code, before it is changed.

use kito_core::paths::Canonical as _;
use kito_core::{LaneId, NoteId};
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

const SHOP: &str = "export function total(prices: number[]): number {\n  let sum = 0;\n  for (const price of prices) {\n    sum += price;\n  }\n  return sum;\n}\n\nexport function discount(amount: number): number {\n  return amount * 0.9;\n}\n";
const TAX: &str = "export function tax(amount: number): number {\n  return amount * 0.2;\n}\n";

fn shop() -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let mut repo = Repo::init(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    write(&root, "src/billing/tax.ts", TAX);
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn noted(repo: &mut Repo, lane: LaneId, target: &str) -> Vec<(String, String)> {
    let graph = repo.why_graph(lane, target, false).unwrap();
    graph.annotations.iter().map(|note| (note.on.clone(), note.text.clone())).collect()
}

const NIGHTLY: &str = "called by the nightly billing job, outside this repository: do not rename";

#[test]
fn a_note_on_a_function_is_told_with_its_why_and_follows_it() {
    let (_tmp, _root, mut repo) = shop();
    let note = repo.add_note(LaneId::MAIN, "src/shop.ts:total", NIGHTLY, None, None).unwrap();
    assert_eq!(
        (note.id.as_str(), note.on.as_str(), note.by.as_str()),
        ("note#1", "src/shop.ts:total", "ada")
    );
    assert_eq!(
        noted(&mut repo, LaneId::MAIN, "src/shop.ts:total"),
        [("src/shop.ts:total".to_owned(), NIGHTLY.to_owned())]
    );
    assert!(noted(&mut repo, LaneId::MAIN, "src/shop.ts:discount").is_empty(), "only on what it is on");

    // Renamed in a lane, and integrated: the note is on it under its new name.
    let lane: LaneId = repo
        .open_lane("bob", None, None, "a clearer name for the total", &kitod::scope::everything())
        .unwrap()
        .id
        .parse()
        .unwrap();
    repo.rename_symbol(lane, "src/shop.ts:total", "grandTotal").unwrap();
    assert_eq!(repo.integrate(lane).unwrap().result, kito_integrate::Outcome::Integrated);
    let notes = noted(&mut repo, LaneId::MAIN, "src/shop.ts:grandTotal");
    assert_eq!(notes, [("src/shop.ts:grandTotal".to_owned(), NIGHTLY.to_owned())]);
}

#[test]
fn notes_on_a_file_and_a_directory_are_told_for_what_is_in_them() {
    let (_tmp, _root, mut repo) = shop();
    repo.add_note(
        LaneId::MAIN,
        "src/billing/",
        "rates come from finance: ask Carla before changing them",
        None,
        None,
    )
    .unwrap();
    repo.add_note(LaneId::MAIN, "src/shop.ts", "the shop is in euros", None, None).unwrap();
    repo.add_note(LaneId::MAIN, "src/shop.ts:5", "integer cents only: floats lose money", None, None)
        .unwrap();

    // Lines are the function they are in.
    let total = noted(&mut repo, LaneId::MAIN, "src/shop.ts:total");
    assert!(
        total.contains(&("src/shop.ts:total".to_owned(), "integer cents only: floats lose money".to_owned()))
    );
    assert!(total.contains(&("src/shop.ts".to_owned(), "the shop is in euros".to_owned())), "{total:?}");
    let tax = noted(&mut repo, LaneId::MAIN, "src/billing/tax.ts:tax");
    assert_eq!(
        tax,
        [("src/billing/".to_owned(), "rates come from finance: ask Carla before changing them".to_owned())]
    );
    // The why of a file has the notes of the functions in it.
    let file = noted(&mut repo, LaneId::MAIN, "src/shop.ts");
    assert_eq!(file.len(), 2, "{file:?}");

    assert_eq!(repo.notes(LaneId::MAIN, None).unwrap().len(), 3);
    assert_eq!(repo.notes(LaneId::MAIN, Some("src/billing/tax.ts")).unwrap().len(), 1);
}

#[test]
fn a_note_is_removed_kept_and_survives_a_restart() {
    let (_tmp, root, mut repo) = shop();
    repo.add_note(LaneId::MAIN, "src/shop.ts:total", NIGHTLY, None, None).unwrap();
    repo.add_note(LaneId::MAIN, "src/shop.ts:discount", "marketing sets the rate", None, None).unwrap();
    let removed = repo.remove_note(NoteId(2), None).unwrap();
    assert_eq!(removed.on, "src/shop.ts:discount");
    assert!(repo.remove_note(NoteId(2), None).unwrap_err().to_string().contains("already removed"));
    assert!(repo.remove_note(NoteId(9), None).unwrap_err().to_string().contains("no such note"));
    drop(repo);

    let mut repo = Repo::open(&root).unwrap();
    let left: Vec<String> = repo.notes(LaneId::MAIN, None).unwrap().into_iter().map(|note| note.id).collect();
    assert_eq!(left, ["note#1"]);
    assert!(noted(&mut repo, LaneId::MAIN, "src/shop.ts:discount").is_empty());
    assert_eq!(repo.add_note(LaneId::MAIN, "src/shop.ts", "a third one", None, None).unwrap().id, "note#3");
}

#[test]
fn a_note_is_asked_for_properly() {
    let (_tmp, _root, mut repo) = shop();
    let error = |repo: &mut Repo, on: &str, text: &str| {
        repo.add_note(LaneId::MAIN, on, text, None, None).unwrap_err().to_string()
    };
    assert!(error(&mut repo, "src/shop.ts:nothing", "a note").contains("nothing called `nothing`"));
    assert!(error(&mut repo, "src/missing.ts", "a note").contains("no such file"));
    assert!(error(&mut repo, "src/none/", "a note").contains("no file is under it"));
    assert!(error(&mut repo, "src/shop.ts", "  ").contains("a few words"));
    assert!(repo.notes(LaneId::MAIN, None).unwrap().is_empty());
}

#[test]
fn an_agent_about_to_change_noted_code_is_told_once() {
    let (_tmp, _root, mut repo) = shop();
    repo.add_note(LaneId::MAIN, "src/shop.ts:total", NIGHTLY, None, None).unwrap();
    let lane: LaneId = repo
        .open_lane("bob", None, None, "taxes on every total", &kitod::scope::everything())
        .unwrap()
        .id
        .parse()
        .unwrap();

    let first = repo.before_edit(Some(lane), Path::new("src/shop.ts"), &[(2, 4)], false).unwrap();
    assert!(!first.allowed && first.kind == "why", "{first:?}");
    assert!(
        first.message.contains("Notes left on this code") && first.message.contains(NIGHTLY),
        "{}",
        first.message
    );
    assert_eq!(first.annotations.len(), 1);
    // Told: the same edit goes through.
    let again = repo.before_edit(Some(lane), Path::new("src/shop.ts"), &[(2, 4)], false).unwrap();
    assert!(again.allowed, "{again:?}");
}
