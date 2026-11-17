//! A lane read and written from another machine: its files and their
//! contents, the saves a writer sends, and what happens when the lane
//! moved on in the meantime.

use kito_core::*;
use kito_integrate::Outcome;
use kitod::files::Put;
use kitod::rpc::{decode, encode};
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
    let info = repo.open_lane(agent, None, None, "work from elsewhere", &scope).unwrap();
    (info.id.parse().unwrap(), info.workdir.unwrap())
}

/// The ID of the content a lane has for a path.
fn blob(repo: &Repo, lane: LaneId, path: &str) -> Option<String> {
    let files = repo.lane_files(lane, Some(&[PathBuf::from(path)])).unwrap();
    files.first().map(|file| file.blob.clone())
}

fn put(result: &str, blob: Option<String>) -> Put {
    Put { result: result.to_owned(), blob }
}

#[test]
fn contents_travel_as_text() {
    for bytes in [&b""[..], b"a", b"ab", b"abc", b"abcd", &[0, 255, 254, 1, 128][..], SHOP.as_bytes()] {
        assert_eq!(decode(&encode(bytes)).as_deref(), Some(bytes));
    }
    assert_eq!(encode(b"kito"), "a2l0bw==");
    assert_eq!(encode(b"lanes"), "bGFuZXM=");
    assert_eq!(decode("bGFuZXM").as_deref(), Some(&b"lanes"[..]), "the padding may be left out");
    assert_eq!(decode("a2l0bw=!"), None);
    assert_eq!(decode("a"), None);
}

#[test]
fn a_lane_is_read_from_elsewhere() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", &["./"]);

    let paths = |repo: &Repo, lane| -> Vec<String> {
        repo.lane_files(lane, None).unwrap().into_iter().map(|file| file.path).collect()
    };
    assert_eq!(paths(&repo, LaneId::MAIN), [".gitignore", "README.md", "src/shop.ts"]);
    assert_eq!(paths(&repo, lane), [".gitignore", "README.md", "src/shop.ts"]);

    // What a lane changes is in its files, not in the main lane's.
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.max(price, 0);"));
    write(&work, "bin/run", "#!/bin/sh\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(work.join("bin/run"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    repo.scan_lane(lane).unwrap();
    assert_eq!(paths(&repo, lane), [".gitignore", "README.md", "bin/run", "src/shop.ts"]);
    assert_ne!(blob(&repo, lane, "src/shop.ts"), blob(&repo, LaneId::MAIN, "src/shop.ts"));
    let files = repo.lane_files(lane, None).unwrap();
    let run = files.iter().find(|file| file.path == "bin/run").unwrap();
    assert!(run.executable && !run.symlink);

    // A content is asked by its ID.
    let id = blob(&repo, lane, "src/shop.ts").unwrap();
    assert!(String::from_utf8(repo.content(&id).unwrap()).unwrap().contains("Math.max(price, 0)"));
    assert!(repo.content("zz").unwrap_err().to_string().contains("not the ID"));
    assert!(repo.content(&"0".repeat(id.len())).unwrap_err().to_string().contains("no content"));

    // Asked about some paths, it tells the ones that exist.
    let some =
        repo.lane_files(lane, Some(&[PathBuf::from("README.md"), PathBuf::from("nothing.ts")])).unwrap();
    assert_eq!(some.len(), 1);
}

#[test]
fn a_save_from_elsewhere_is_recorded_as_the_lane_s_own() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", &["src/shop.ts:total", "src/tax.ts"]);
    let base = blob(&repo, lane, "src/shop.ts").unwrap();
    let ops_before = repo.lane_ops(lane).unwrap();

    let edited = SHOP.replace("sum += price;", "sum += Math.max(price, 0);");
    let answer = repo.put_file(lane, "src/shop.ts", edited.as_bytes(), false, Some(&base)).unwrap();
    assert_eq!(answer.result, "recorded");
    assert_eq!(answer.blob, blob(&repo, lane, "src/shop.ts"));
    assert_eq!(read(&work, "src/shop.ts"), edited, "the working copy on the server has it");
    assert!(repo.lane_ops(lane).unwrap() > ops_before);

    // It is recorded as the agent's, with the lane's intent, function by function.
    let log = repo.log_lane(lane, 10, None).unwrap();
    let text = format!("{log:?}");
    assert!(text.contains("total"), "{text}");
    assert!(text.contains("alice"), "{text}");
    assert!(text.contains("work from elsewhere"), "{text}");

    // The same save sent again (the answer was lost on the way) is one save.
    let ops = repo.lane_ops(lane).unwrap();
    let again = repo.put_file(lane, "src/shop.ts", edited.as_bytes(), false, Some(&base)).unwrap();
    assert_eq!(again, answer);
    assert_eq!(repo.lane_ops(lane).unwrap(), ops);

    // A new file has no base.
    let tax = "export function tax(amount: number): number {\n  return amount * 0.22;\n}\n";
    let answer = repo.put_file(lane, "src/tax.ts", tax.as_bytes(), false, None).unwrap();
    assert_eq!(answer.result, "recorded");
    assert_eq!(read(&work, "src/tax.ts"), tax);

    // And the lane integrates as any other.
    let result = repo.integrate(lane).unwrap();
    assert_eq!(result.result, Outcome::Integrated, "{result:?}");
    assert!(read(repo.root(), "src/shop.ts").contains("Math.max(price, 0)"));
    assert!(read(repo.root(), "src/tax.ts").contains("0.22"));
}

#[test]
fn a_save_made_on_an_older_version_is_merged_or_refused() {
    let (_tmp, _root, mut repo) = shop();
    let (alice, _) = open(&mut repo, "alice", &["src/shop.ts:total"]);
    let (bob, bob_work) = open(&mut repo, "bob", &["src/shop.ts:discount"]);
    let old = blob(&repo, alice, "src/shop.ts").unwrap();

    // Bob's change reaches the main lane, and alice's lane with her next
    // integration: her copy elsewhere still has the file as it was.
    write(&bob_work, "src/shop.ts", &SHOP.replace("/ 100);", "/ 100.0);"));
    assert_eq!(repo.integrate(bob).unwrap().result, Outcome::Integrated);
    let first = SHOP.replace("let sum = 0;", "let sum = 0; // cents");
    assert_eq!(
        repo.put_file(alice, "src/shop.ts", first.as_bytes(), false, Some(&old)).unwrap().result,
        "recorded"
    );
    assert_eq!(repo.integrate(alice).unwrap().result, Outcome::Integrated);
    let moved = blob(&repo, alice, "src/shop.ts").unwrap();
    assert!(String::from_utf8(repo.content(&moved).unwrap()).unwrap().contains("/ 100.0);"));

    // She saves again, on the version she sent: it merges with bob's change.
    let second = first.replace("return sum;", "return Math.round(sum);");
    let sent = BlobId::of(first.as_bytes()).to_string();
    let answer = repo.put_file(alice, "src/shop.ts", second.as_bytes(), false, Some(&sent)).unwrap();
    assert_eq!(answer.result, "merged");
    let merged = String::from_utf8(repo.content(answer.blob.as_ref().unwrap()).unwrap()).unwrap();
    assert!(merged.contains("return Math.round(sum);") && merged.contains("/ 100.0);"), "{merged}");
    assert_eq!(read(&repo.lane_info(alice).unwrap().workdir.unwrap(), "src/shop.ts"), merged);

    // A save that changes the very line the lane changed since does not
    // merge: nothing is touched, and the answer says what the lane has.
    let now = blob(&repo, alice, "src/shop.ts").unwrap();
    let ops = repo.lane_ops(alice).unwrap();
    let clash = first.replace("/ 100);", "/ 50);");
    let answer = repo.put_file(alice, "src/shop.ts", clash.as_bytes(), false, Some(&sent)).unwrap();
    assert_eq!(answer, put("conflict", Some(now.clone())));
    assert_eq!(blob(&repo, alice, "src/shop.ts"), Some(now.clone()));
    assert_eq!(repo.lane_ops(alice).unwrap(), ops);

    // So does a file that exists on both sides with nothing in common,
    assert_eq!(
        repo.put_file(alice, "src/shop.ts", b"new\n", false, None).unwrap(),
        put("conflict", Some(now))
    );
    // and a change to a file the lane no longer has.
    let readme = blob(&repo, alice, "README.md").unwrap();
    assert_eq!(repo.remove_file(alice, "README.md", Some(&readme)).unwrap(), put("recorded", None));
    assert_eq!(
        repo.put_file(alice, "README.md", b"# Shop, again\n", false, Some(&readme)).unwrap(),
        put("conflict", None)
    );
}

#[test]
fn a_file_changed_on_the_server_s_own_disk_is_not_overwritten() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", &["./"]);
    let base = blob(&repo, lane, "src/shop.ts").unwrap();

    // Changed in the working copy on the server, and not recorded yet
    // when a save made elsewhere on the old version arrives.
    write(&work, "src/shop.ts", &SHOP.replace("/ 100);", "/ 100.0);"));
    let sent = SHOP.replace("let sum = 0;", "let sum = 0; // cents");
    let answer = repo.put_file(lane, "src/shop.ts", sent.as_bytes(), false, Some(&base)).unwrap();
    assert_eq!(answer.result, "merged");
    let now = read(&work, "src/shop.ts");
    assert!(now.contains("/ 100.0);") && now.contains("// cents"), "{now}");

    // The same for a removal: the file changed here stays.
    let readme = blob(&repo, lane, "README.md").unwrap();
    write(&work, "README.md", "# Shop\n\nOpen every day.\n");
    assert_eq!(repo.remove_file(lane, "README.md", Some(&readme)).unwrap().result, "conflict");
    assert!(read(&work, "README.md").contains("Open every day."));
}

#[test]
fn a_removal_from_elsewhere() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", &["./"]);
    let readme = blob(&repo, lane, "README.md").unwrap();

    // The lane changed the file since: it stays.
    write(&work, "README.md", "# Shop\n\nOpen every day.\n");
    repo.scan_lane(lane).unwrap();
    let now = blob(&repo, lane, "README.md").unwrap();
    assert_eq!(
        repo.remove_file(lane, "README.md", Some(&readme)).unwrap(),
        put("conflict", Some(now.clone()))
    );
    assert!(work.join("README.md").exists());

    // Removed from the version the lane has, it goes; twice is once.
    assert_eq!(repo.remove_file(lane, "README.md", Some(&now)).unwrap(), put("recorded", None));
    assert!(!work.join("README.md").exists());
    assert_eq!(blob(&repo, lane, "README.md"), None);
    assert_eq!(repo.remove_file(lane, "README.md", Some(&now)).unwrap(), put("recorded", None));
    let text = format!("{:?}", repo.log_lane(lane, 5, None).unwrap());
    assert!(text.contains("README.md"), "{text}");
}

#[test]
fn what_is_not_written_from_elsewhere() {
    let (_tmp, root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", &["./"]);
    let refused = |r: kitod::error::Result<Put>| r.unwrap_err().to_string();

    assert!(refused(repo.put_file(LaneId::MAIN, "README.md", b"x", false, None)).contains("open a lane"));
    assert!(refused(repo.remove_file(LaneId::MAIN, "README.md", None)).contains("open a lane"));
    assert_eq!(read(&root, "README.md"), "# Shop\n");

    for path in ["../outside.ts", "/etc/passwd", "src/../../x", ""] {
        assert!(
            refused(repo.put_file(lane, path, b"x", false, None)).contains("not a path inside"),
            "{path}"
        );
    }
    for path in [".kito/config.toml", ".git/config", "node_modules/a/index.js", "debug.log"] {
        assert!(refused(repo.put_file(lane, path, b"x", false, None)).contains("ignored"), "{path}");
        assert!(!work.join(path).exists(), "{path}");
    }
    assert!(!root.parent().unwrap().join("outside.ts").exists());

    // A lane is written by the person it was opened for.
    repo.set_lane_owner(lane, Author::human("francesca")).unwrap();
    assert!(repo.may_write(lane, Some("francesca")).is_ok());
    assert!(repo.may_write(lane, None).is_ok(), "on the machine of the repository, anyone who can reach it");
    let error = repo.may_write(lane, Some("marco")).unwrap_err().to_string();
    assert!(error.contains("francesca's"), "{error}");
}
