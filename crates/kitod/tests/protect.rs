//! A protected main lane: it changes when a lane is integrated, and in no
//! other way.

use kito_core::*;
use kito_integrate::Outcome;
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
  const cut = Math.round((amount * percent) / 100);
  return amount - cut;
}
"#;

fn shop() -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    drop(Repo::init(&root).unwrap());
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\nnode_modules/\n*.log\n");
    write(&root, "src/shop.ts", SHOP);
    write(&root, "README.md", "# Shop\n");
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn open(repo: &mut Repo, agent: &str, items: &[&str]) -> (LaneId, PathBuf) {
    let scope: Vec<String> = items.iter().map(|item| (*item).to_owned()).collect();
    let info = repo.open_lane(agent, None, None, "a change that goes through a lane", &scope).unwrap();
    (info.id.parse().unwrap(), info.workdir.unwrap())
}

/// Every file kept aside, as paths under `.kito/refused/<when>/`.
fn kept(root: &Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for when in fs::read_dir(root.join(".kito/refused")).into_iter().flatten().flatten() {
        let mut dirs = vec![when.path()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                match entry.file_type().unwrap().is_dir() {
                    true => dirs.push(entry.path()),
                    false => {
                        let rel = entry.path().strip_prefix(when.path()).unwrap().display().to_string();
                        found.push((rel, fs::read_to_string(entry.path()).unwrap()));
                    }
                }
            }
        }
    }
    found.sort();
    found
}

#[test]
fn a_direct_edit_to_a_protected_main_lane_is_put_back_and_kept_aside() {
    let (_tmp, root, mut repo) = shop();
    assert!(repo.protect(true).unwrap());
    assert!(!repo.protect(true).unwrap(), "twice is once");
    let ops = repo.status().unwrap().ops;

    // Changed, created and removed in the main directory: none of it is
    // recorded, and the directory is the main lane again.
    write(&root, "README.md", "# Shop\n\nEdited where nobody should.\n");
    write(&root, "src/new.ts", "export const stray = 1;\n");
    fs::remove_file(root.join("src/shop.ts")).unwrap();
    // What Kito does not track is nobody's business.
    write(&root, "node_modules/left-pad/index.js", "module.exports = 1;\n");
    write(&root, "debug.log", "noise\n");
    assert!(repo.scan().unwrap().is_empty());
    assert_eq!(read(&root, "README.md"), "# Shop\n");
    assert!(!root.join("src/new.ts").exists());
    assert_eq!(read(&root, "src/shop.ts"), SHOP);
    assert!(root.join("node_modules/left-pad/index.js").is_file() && root.join("debug.log").is_file());
    assert_eq!(repo.status().unwrap().ops, ops);

    // Nothing anybody typed is thrown away.
    assert_eq!(
        kept(&root),
        [
            ("README.md".to_owned(), "# Shop\n\nEdited where nobody should.\n".to_owned()),
            ("src/new.ts".to_owned(), "export const stray = 1;\n".to_owned()),
        ]
    );
    let status = repo.status().unwrap();
    assert!(status.protected);
    assert_eq!(status.refused, 1);
    let last = status.last_refused.unwrap();
    assert_eq!(last.paths, ["README.md", "src/new.ts", "src/shop.ts"]);
    assert!(last.kept.unwrap().starts_with(root.join(".kito/refused")));

    // Looking again finds nothing to put back.
    assert!(repo.scan().unwrap().is_empty());
    assert_eq!(repo.status().unwrap().refused, 1);

    // The setting is in the configuration, among what was already there.
    let config = read(&root, ".kito/config.toml");
    assert!(config.contains("protected = true") && config.contains("# Kito configuration"), "{config}");
    drop(repo);
    let repo = Repo::open(&root).unwrap();
    assert!(repo.status().unwrap().protected, "it holds for the next daemon too");
}

#[test]
fn a_lane_is_the_way_in() {
    let (_tmp, root, mut repo) = shop();
    repo.protect(true).unwrap();
    let (lane, work) = open(&mut repo, "alice", &["src/shop.ts:total"]);
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.max(price, 0);"));

    // Somebody edits the same file in the main directory meanwhile: the
    // integration puts that back, and brings in the lane's work.
    write(&root, "src/shop.ts", &SHOP.replace("/ 100);", "/ 1000);"));
    let result = repo.integrate(lane).unwrap();
    assert_eq!(result.result, Outcome::Integrated, "{result:?}");
    let now = read(&root, "src/shop.ts");
    assert!(now.contains("Math.max(price, 0)") && now.contains("/ 100);"), "{now}");
    assert_eq!(kept(&root).len(), 1);
    assert!(kept(&root)[0].1.contains("/ 1000);"));

    // The integration is a checkpoint, by the agent, and nothing is left
    // to put in another.
    let status = repo.status().unwrap();
    assert_eq!((status.checkpoints, status.ops_since_checkpoint), (2, 0));
    let said = repo.checkpoint("by hand").unwrap_err().to_string();
    assert!(said.contains("every integration is a checkpoint"), "{said}");
}

#[test]
fn what_a_protected_main_lane_refuses_and_how_it_is_opened_again() {
    let (_tmp, root, mut repo) = shop();
    write(&root, "README.md", "# Shop\n\nOpen every day.\n");
    repo.checkpoint("opening hours").unwrap();

    // Turned on with something in the directory that is in no checkpoint:
    // that state gets one, as the last put there by hand.
    write(&root, "README.md", "# Shop\n\nOpen every day but Monday.\n");
    repo.protect(true).unwrap();
    let status = repo.status().unwrap();
    assert_eq!((status.checkpoints, status.ops_since_checkpoint), (3, 0));
    assert_eq!(read(&root, "README.md"), "# Shop\n\nOpen every day but Monday.\n");

    let refused = |e: kitod::error::Error| {
        let said = e.to_string();
        assert!(said.contains("the main lane is protected") && said.contains("kito protect --off"), "{said}");
    };
    refused(repo.rollback("ckpt#1", &[], false).unwrap_err());
    refused(repo.rename_symbol_with(LaneId::MAIN, "src/shop.ts:total", "sum", None).unwrap_err());
    // Looking at what a rollback would do changes nothing, and is allowed.
    assert!(!repo.rollback("ckpt#1", &[], true).unwrap().changes.is_empty());
    assert_eq!(read(&root, "README.md"), "# Shop\n\nOpen every day but Monday.\n");

    // In a lane the same rename is welcome.
    let (lane, _work) = open(&mut repo, "alice", &["src/shop.ts:total"]);
    repo.rename_symbol_with(lane, "src/shop.ts:total", "sum", None).unwrap();
    assert_eq!(repo.integrate(lane).unwrap().result, Outcome::Integrated);
    assert!(read(&root, "src/shop.ts").contains("export function sum("));

    // Turned off, the main directory is edited by hand as before.
    assert!(repo.protect(false).unwrap());
    write(&root, "README.md", "# Shop\n");
    assert!(!repo.scan().unwrap().is_empty());
    repo.checkpoint("by hand again").unwrap();
    assert!(repo.rollback("ckpt#1", &[], false).is_ok());
}
