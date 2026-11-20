//! The why of a file that was renamed: its lines and reasons from before
//! the rename are followed under its earlier names (phase 18).

use kito_core::paths::Canonical as _;
use kito_core::LaneId;
use kito_integrate::Outcome;
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

const SHOP: &str = "export function total(prices: number[]): number {\n  let sum = 0;\n  for (const price of prices) {\n    sum += price;\n  }\n  return sum;\n}\n";
const CENTS: &str = "export function total(prices: number[]): number {\n  let sum = 0;\n  for (const price of prices) {\n    sum += Math.round(price * 100);\n  }\n  return sum;\n}\n";

fn shop() -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let mut repo = Repo::init(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    repo.checkpoint("start").unwrap();
    // A lane changes a line for a reason.
    let lane: LaneId = repo
        .open_lane("alice", None, None, "prices in cents", &kitod::scope::everything())
        .unwrap()
        .id
        .parse()
        .unwrap();
    write(Path::new(&repo.lane_info(lane).unwrap().workdir.unwrap()), "src/shop.ts", CENTS);
    assert_eq!(repo.integrate(lane).unwrap().result, Outcome::Integrated);
    repo.close_lane(lane).unwrap();
    (tmp, root, repo)
}

fn reason_of_line(repo: &mut Repo, target: &str, line: u32) -> Option<String> {
    let graph = repo.why_graph(LaneId::MAIN, target, false).unwrap();
    let run = graph.lines.iter().find(|run| run.from <= line && line <= run.to)?;
    run.reason.map(|reason| graph.reasons[reason].why.clone())
}

#[test]
fn the_lines_of_a_renamed_file_keep_their_reasons() {
    let (_tmp, root, mut repo) = shop();
    assert_eq!(reason_of_line(&mut repo, "src/shop.ts:4", 4).as_deref(), Some("prices in cents"));

    // Renamed on the main lane, as it is: the lines keep their reasons.
    fs::rename(root.join("src/shop.ts"), root.join("src/store.ts")).unwrap();
    repo.checkpoint("the shop is a store").unwrap();
    assert_eq!(reason_of_line(&mut repo, "src/store.ts:4", 4).as_deref(), Some("prices in cents"));
    let file = repo.why_graph(LaneId::MAIN, "src/store.ts", false).unwrap();
    let whys: Vec<&str> = file.reasons.iter().map(|reason| reason.why.as_str()).collect();
    assert!(whys.contains(&"prices in cents"), "{whys:?}");
    assert!(file.notes.iter().all(|note| !note.contains("older than")), "{:?}", file.notes);

    // Twice: followed through both names.
    fs::create_dir_all(root.join("lib")).unwrap();
    fs::rename(root.join("src/store.ts"), root.join("lib/store.ts")).unwrap();
    repo.checkpoint("the store is a library").unwrap();
    assert_eq!(reason_of_line(&mut repo, "lib/store.ts:4", 4).as_deref(), Some("prices in cents"));
}

#[test]
fn a_new_file_under_an_old_name_does_not_take_its_history() {
    let (_tmp, root, mut repo) = shop();
    fs::rename(root.join("src/shop.ts"), root.join("src/store.ts")).unwrap();
    repo.checkpoint("the shop is a store").unwrap();
    // Another file, later, under the name the first one had.
    write(&root, "src/shop.ts", "export const opening = \"9:00\";\n");
    repo.checkpoint("opening hours").unwrap();
    let file = repo.why_graph(LaneId::MAIN, "src/shop.ts", false).unwrap();
    let whys: Vec<&str> = file.reasons.iter().map(|reason| reason.why.as_str()).collect();
    assert!(!whys.contains(&"prices in cents"), "{whys:?}");
    // And the renamed file keeps its own.
    assert_eq!(reason_of_line(&mut repo, "src/store.ts:4", 4).as_deref(), Some("prices in cents"));
}
