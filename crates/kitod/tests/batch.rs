//! Lanes that ask to be integrated at the same time: the ones that do not
//! depend on each other are checked in one run of the repository's checks.

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
        // Nothing here confines the checks: they count their runs in a
        // file next to the repository.
        std::env::remove_var("KITOD_BIN");
    });
}

const SHOP: &str = "export function total(prices: number[]): number {\n  return prices.length;\n}\n\nexport function discount(amount: number): number {\n  return amount;\n}\n";

/// A repository with six files and a check that counts its runs and
/// fails if any file says BROKEN. `batch`: how many lanes a run may take.
fn shop(batch: usize) -> (tempfile::TempDir, PathBuf, PathBuf, Repo) {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("shop");
    fs::create_dir(&root).unwrap();
    drop(Repo::init(&root).unwrap());
    let runs = base.join("runs.txt");
    let check = format!("echo run >> {}; ! grep -rq BROKEN --include=*.txt --include=*.ts .", runs.display());
    let config = root.join(".kito/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    let text = text
        .replace("# commands = [\"npx tsc --noEmit\", \"npm test\"]", &format!("commands = [{check:?}]"))
        .replace("batch = 8", &format!("batch = {batch}"));
    assert!(text.contains("echo run"), "{text}");
    fs::write(&config, text).unwrap();
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    for name in ["a", "b", "c", "d"] {
        write(&root, &format!("docs/{name}.txt"), &format!("{name}: one\n"));
    }
    write(&root, "src/shop.ts", SHOP);
    repo.checkpoint("start").unwrap();
    (tmp, root, runs, repo)
}

fn runs(file: &Path) -> usize {
    fs::read_to_string(file).unwrap_or_default().lines().count()
}

/// Opens a lane on one file and changes it there.
fn lane(repo: &mut Repo, agent: &str, file: &str, content: &str) -> LaneId {
    let scope = [file.to_owned()];
    let info = repo.open_lane(agent, None, None, &format!("{agent} on {file}"), &scope).unwrap();
    write(info.workdir.as_ref().unwrap(), file, content);
    info.id.parse().unwrap()
}

#[test]
fn lanes_that_do_not_depend_on_each_other_are_checked_in_one_run() {
    let (_tmp, root, counted, mut repo) = shop(8);
    assert_eq!(repo.batch_limit(), 8);
    let lanes: Vec<LaneId> = ["a", "b", "c"]
        .iter()
        .map(|n| lane(&mut repo, n, &format!("docs/{n}.txt"), &format!("{n}: two\n")))
        .collect();
    let before = repo.status().unwrap().checkpoints;

    let results = repo.integrate_together(&lanes).unwrap();
    assert_eq!(runs(&counted), 1, "one run of the checks for the three of them");
    assert_eq!(results.len(), 3);
    for (lane, result) in &results {
        assert_eq!(result.result, Outcome::Integrated, "{lane}: {result:?}");
        let report = result.verification.as_ref().unwrap();
        assert!(report.passed);
        assert_eq!(report.batch, ["lane/1", "lane/2", "lane/3"]);
    }
    // In the order they asked, each under a checkpoint of its own.
    assert_eq!(results.iter().map(|(lane, _)| *lane).collect::<Vec<_>>(), lanes);
    assert_eq!(repo.status().unwrap().checkpoints, before + 3);
    for name in ["a", "b", "c"] {
        assert_eq!(read(&root, &format!("docs/{name}.txt")), format!("{name}: two\n"));
    }
    assert_eq!(read(&root, "docs/d.txt"), "d: one\n");

    // A repository may want each lane checked by itself.
    let (_tmp, _root, counted, mut repo) = shop(1);
    assert_eq!(repo.batch_limit(), 1);
    let lanes: Vec<LaneId> = ["a", "b"]
        .iter()
        .map(|n| lane(&mut repo, n, &format!("docs/{n}.txt"), &format!("{n}: two\n")))
        .collect();
    for lane in &lanes {
        assert_eq!(repo.integrate(*lane).unwrap().result, Outcome::Integrated);
    }
    assert_eq!(runs(&counted), 2);
}

#[test]
fn a_run_that_fails_is_split_until_the_lane_that_fails_is_found() {
    let (_tmp, root, counted, mut repo) = shop(8);
    let mut lanes = Vec::new();
    for (name, content) in [("a", "a: two\n"), ("b", "b: two\n"), ("c", "c: BROKEN\n"), ("d", "d: two\n")] {
        lanes.push(lane(&mut repo, name, &format!("docs/{name}.txt"), content));
    }
    let results = repo.integrate_together(&lanes).unwrap();
    let of = |lane: LaneId| &results.iter().find(|(l, _)| *l == lane).unwrap().1;
    assert_eq!(of(lanes[0]).result, Outcome::Integrated);
    assert_eq!(of(lanes[1]).result, Outcome::Integrated);
    assert_eq!(of(lanes[3]).result, Outcome::Integrated);
    let failed = of(lanes[2]);
    assert_eq!(failed.result, Outcome::VerificationFailed, "{failed:?}");
    assert!(failed.verification.as_ref().unwrap().batch.is_empty(), "it failed by itself");
    // The four together, the first two, the last two, then each of those.
    assert_eq!(runs(&counted), 5);
    assert_eq!(read(&root, "docs/c.txt"), "c: one\n", "what fails is not integrated");
    assert_eq!(read(&root, "docs/a.txt"), "a: two\n");
    assert_eq!(read(&root, "docs/d.txt"), "d: two\n");
    // The two that passed together say so.
    assert_eq!(of(lanes[0]).verification.as_ref().unwrap().batch, ["lane/1", "lane/2"]);
}

#[test]
fn lanes_that_depend_on_each_other_are_checked_one_by_one() {
    let (_tmp, root, counted, mut repo) = shop(8);
    // Two lanes on two functions of one file: they merge, function by
    // function, but what one writes depends on what the other wrote.
    let scope = ["src/shop.ts:total".to_owned()];
    let one = repo.open_lane("one", None, None, "sum the prices", &scope).unwrap();
    write(
        one.workdir.as_ref().unwrap(),
        "src/shop.ts",
        &SHOP.replace("prices.length", "prices.reduce((a, b) => a + b, 0)"),
    );
    let scope = ["src/shop.ts:discount".to_owned()];
    let two = repo.open_lane("two", None, None, "ten off", &scope).unwrap();
    write(
        two.workdir.as_ref().unwrap(),
        "src/shop.ts",
        &SHOP.replace("return amount;", "return amount - 10;"),
    );
    // A lane that renames: the rename may reach what the others wrote.
    let scope = ["docs/a.txt".to_owned()];
    let three = repo.open_lane("three", None, None, "a note", &scope).unwrap();
    write(three.workdir.as_ref().unwrap(), "docs/a.txt", "a: two\n");
    let ids: Vec<LaneId> = [&one, &two, &three].iter().map(|lane| lane.id.parse().unwrap()).collect();

    let plan = repo.batch_plan(&ids).unwrap();
    assert_eq!(plan.members.iter().map(|member| member.lane).collect::<Vec<_>>(), [ids[0], ids[2]]);
    assert_eq!(plan.alone, [ids[1]], "the second lane on the file waits for the first");

    let results = repo.integrate_together(&ids).unwrap();
    assert!(results.iter().all(|(_, result)| result.result == Outcome::Integrated), "{results:?}");
    assert_eq!(runs(&counted), 2, "one run for the two that stand apart, one for the other");
    let merged = read(&root, "src/shop.ts");
    assert!(merged.contains("prices.reduce") && merged.contains("amount - 10"), "{merged}");
}

#[test]
fn a_lane_that_changed_since_it_was_checked_is_not_integrated_on_that_check() {
    let (_tmp, root, counted, mut repo) = shop(8);
    let a = lane(&mut repo, "a", "docs/a.txt", "a: two\n");
    let b = lane(&mut repo, "b", "docs/b.txt", "b: two\n");
    let plan = repo.batch_plan(&[a, b]).unwrap();
    assert_eq!(plan.members.len(), 2);
    let checks = plan.checks.as_ref().unwrap();
    assert_eq!(read(&checks.dir, "docs/a.txt"), "a: two\n");
    assert_eq!(read(&checks.dir, "docs/b.txt"), "b: two\n");
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let report = runtime.block_on(checks.run());
    assert!(report.passed);

    // The first is what was checked: it goes in.
    let landed = repo.land_member(&plan.members[0], report.clone(), None).unwrap();
    assert_eq!(landed.unwrap().result, Outcome::Integrated);
    // The second was saved again meanwhile: not on this check.
    let work = root.join(".kito/lanes/2/work");
    write(&work, "docs/b.txt", "b: BROKEN\n");
    assert!(repo.land_member(&plan.members[1], report, None).unwrap().is_none());
    assert_eq!(read(&root, "docs/b.txt"), "b: one\n");
    // By itself it is checked as it is now, and fails.
    assert_eq!(repo.integrate(b).unwrap().result, Outcome::VerificationFailed);
    assert_eq!(runs(&counted), 2);
}
