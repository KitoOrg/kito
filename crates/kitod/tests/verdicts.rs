//! Review of a lane (ADR 0004): verdicts, and what integration asks.

use kito_core::paths::Canonical as _;
use kito_core::*;
use kito_integrate::Outcome;
use kitod::lanes::Stack;
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
"#;
const CENTS: &str = "sum += Math.round(price * 100);";

/// A repository whose lanes need `required` approvals, and more of
/// `[review]` as written.
fn shop(review: &str) -> (tempfile::TempDir, PathBuf, Repo) {
    isolate_identity();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    drop(Repo::init(&root).unwrap());
    let config = root.join(".kito/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    let set = text.replace("[review]\n", &format!("[review]\n{review}\n"));
    // What is written under the table comes before the defaults: the
    // defaults go, so that no key is there twice.
    let set: String = set
        .lines()
        .filter(|line| {
            !["required = 0", "agents = false", "keep = false"].contains(line) || review.is_empty()
        })
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&config, set).unwrap();
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

/// A lane of alice's agent, opened for ada, with the prices in cents.
fn lane_in_cents(repo: &mut Repo) -> (LaneId, PathBuf) {
    let info = repo.open_lane("alice", None, None, "prices in cents", &["./".to_owned()]).unwrap();
    let (lane, work): (LaneId, PathBuf) = (info.id.parse().unwrap(), info.workdir.unwrap());
    repo.set_lane_owner(lane, Author::human("ada")).unwrap();
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", CENTS));
    repo.scan_lane(lane).unwrap();
    (lane, work)
}

fn person(name: &str) -> Option<Author> {
    Some(Author::human(name).with_email(format!("{name}@example.com")))
}

fn approve(repo: &mut Repo, lane: LaneId, name: &str) -> kitod::verdicts::ReviewInfo {
    repo.give_verdict(lane, VerdictKind::Approved, None, person(name), None).unwrap()
}

#[test]
fn a_lane_that_needs_an_approval_waits_for_it_and_approving_brings_nothing_in() {
    let (_tmp, root, mut repo) = shop("required = 1");
    let (lane, _work) = lane_in_cents(&mut repo);
    assert_eq!(repo.lane_info(lane).unwrap().review.as_deref(), Some("0 of 1 approval"));

    // Merged and checked, it waits: nothing is written.
    let asked = repo.integrate(lane).unwrap();
    assert_eq!(asked.result, Outcome::AwaitingApproval, "{asked:?}");
    assert_eq!(asked.missing.as_deref(), Some("0 of 1 approval"));
    assert!(asked.message.unwrap().contains("lacks its review (0 of 1 approval)"));
    assert_eq!(read(&root, "src/shop.ts"), SHOP);
    assert_eq!(repo.lane_info(lane).unwrap().state, "awaiting_approval");

    // Bob approves. That is his word, and only that: the main lane is as
    // it was, and the lane says it has what it needs.
    let review = approve(&mut repo, lane, "bob");
    assert_eq!(
        (review.required, review.approved_by.as_slice(), review.missing),
        (1, &["bob".to_owned()][..], None)
    );
    assert_eq!(read(&root, "src/shop.ts"), SHOP, "an approval brings nothing in");
    assert_eq!(repo.status().unwrap().checkpoints, 1);
    let info = repo.lane_info(lane).unwrap();
    assert_eq!((info.state.as_str(), info.review.as_deref()), ("awaiting_approval", Some("approved by bob")));

    // Integrating is its own action, and now goes through. The checkpoint
    // says who reviewed, and so does the why of that code.
    let done = repo.integrate(lane).unwrap();
    assert_eq!((done.result, done.missing), (Outcome::Integrated, None));
    assert!(read(&root, "src/shop.ts").contains(CENTS));
    let checkpoint = repo.store().checkpoints().next_back().unwrap().clone();
    assert_eq!(checkpoint.reviewed_by, [person("bob").unwrap()]);
    let why = repo.why_graph(LaneId::MAIN, "src/shop.ts:total", false).unwrap();
    assert_eq!(why.reasons[0].reviewed_by, ["bob"]);
}

#[test]
fn an_approval_is_of_the_lane_as_it_was() {
    let (_tmp, _root, mut repo) = shop("required = 1");
    let (lane, work) = lane_in_cents(&mut repo);
    approve(&mut repo, lane, "bob");

    // The lane changes: what bob approved is no longer what it holds.
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.floor(price * 100);"));
    repo.scan_lane(lane).unwrap();
    let review = repo.review(lane).unwrap();
    assert_eq!(review.missing.as_deref(), Some("0 of 1 approval"));
    let verdict = &review.verdicts[0];
    assert!(verdict.stale && !verdict.counts, "{verdict:?}");
    assert_eq!(verdict.because.as_deref(), Some("the lane changed since"));
    assert_eq!(repo.integrate(lane).unwrap().result, Outcome::AwaitingApproval);

    // Approved again as it is now, it goes in.
    approve(&mut repo, lane, "bob");
    assert_eq!(repo.integrate(lane).unwrap().result, Outcome::Integrated);
}

#[test]
fn approvals_are_kept_where_the_repository_says_so() {
    let (_tmp, _root, mut repo) = shop("required = 1\nkeep = true");
    let (lane, work) = lane_in_cents(&mut repo);
    approve(&mut repo, lane, "bob");
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.floor(price * 100);"));
    repo.scan_lane(lane).unwrap();
    let review = repo.review(lane).unwrap();
    assert!(review.verdicts[0].stale && review.verdicts[0].counts && review.missing.is_none(), "{review:?}");
    assert_eq!(repo.integrate(lane).unwrap().result, Outcome::Integrated);
}

#[test]
fn a_request_for_changes_stops_a_lane_until_it_is_taken_back() {
    // Also where no approval is needed.
    let (_tmp, root, mut repo) = shop("");
    let (lane, work) = lane_in_cents(&mut repo);
    assert_eq!(repo.lane_info(lane).unwrap().review, None);
    let said = repo.give_verdict(lane, VerdictKind::ChangesRequested, None, person("bob"), None).unwrap_err();
    assert!(said.to_string().contains("say what should change"), "{said}");
    let review = repo
        .give_verdict(
            lane,
            VerdictKind::ChangesRequested,
            Some("Round half up, not down."),
            person("bob"),
            None,
        )
        .unwrap();
    assert_eq!(review.missing.as_deref(), Some("changes requested by bob"));
    assert_eq!(repo.integrate(lane).unwrap().missing.as_deref(), Some("changes requested by bob"));

    // It stands however the lane changes, and whoever else approves.
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.round(price * 100 + 0.0);"));
    repo.scan_lane(lane).unwrap();
    approve(&mut repo, lane, "carol");
    let review = repo.review(lane).unwrap();
    assert_eq!(
        (review.approved_by.as_slice(), review.missing.as_deref()),
        (&["carol".to_owned()][..], Some("changes requested by bob"))
    );
    assert_eq!(repo.lane_info(lane).unwrap().review.as_deref(), Some("changes requested by bob"));

    // One verdict a person: bob's approval takes the place of his request.
    let review = approve(&mut repo, lane, "bob");
    assert_eq!((review.verdicts.len(), review.missing), (2, None));
    assert_eq!(review.approved_by, ["carol", "bob"]);
    // Taken back, a verdict is as if it had not been given.
    let review = repo.give_verdict(lane, VerdictKind::Withdrawn, None, person("bob"), None).unwrap();
    assert_eq!(review.approved_by, ["carol"]);
    let none = repo.give_verdict(lane, VerdictKind::Withdrawn, None, person("dave"), None).unwrap_err();
    assert!(none.to_string().contains("dave gave no verdict"), "{none}");

    assert_eq!(repo.integrate(lane).unwrap().result, Outcome::Integrated);
    assert!(read(&root, "src/shop.ts").contains("+ 0.0"));
}

#[test]
fn a_verdict_is_somebody_else_s_and_a_person_s() {
    let (_tmp, _root, mut repo) = shop("required = 1");
    let (lane, _work) = lane_in_cents(&mut repo);
    let refused = |result: kitod::Result<kitod::verdicts::ReviewInfo>, words: &str| {
        let said = result.unwrap_err().to_string();
        assert!(said.contains(words), "{said}");
    };
    // The owner's approval is given, and does not count: a review is
    // somebody else's. Their request for changes stands.
    let own = repo.give_verdict(lane, VerdictKind::Approved, None, person("ada"), None).unwrap();
    assert_eq!((own.approved_by.len(), own.missing.as_deref()), (0, Some("0 of 1 approval")));
    assert_eq!(own.verdicts[0].because.as_deref(), Some("the lane is theirs"));
    let own = repo
        .give_verdict(lane, VerdictKind::ChangesRequested, Some("Not yet."), person("ada"), None)
        .unwrap();
    assert_eq!(own.missing.as_deref(), Some("0 of 1 approval; changes requested by ada"));
    repo.give_verdict(lane, VerdictKind::Withdrawn, None, person("ada"), None).unwrap();
    // Not an agent's, unless the repository says so.
    let reviewer = Some(Author::agent("reviewer"));
    refused(
        repo.give_verdict(lane, VerdictKind::Approved, None, reviewer.clone(), person("bob")),
        "a verdict is a person's word",
    );
    refused(
        repo.give_verdict(LaneId::MAIN, VerdictKind::Approved, None, person("bob"), None),
        "not an open lane",
    );
    assert!(repo.review(LaneId::MAIN).unwrap_err().to_string().contains("the main lane is not reviewed"));
    assert_eq!(repo.review(lane).unwrap().verdicts.len(), 0);

    // Where agents may: it counts as its person's, once.
    let (_tmp, _root, mut repo) = shop("required = 2\nagents = true");
    let (lane, _work) = lane_in_cents(&mut repo);
    repo.give_verdict(lane, VerdictKind::Approved, None, reviewer.clone(), person("bob")).unwrap();
    let review = approve(&mut repo, lane, "bob");
    assert_eq!(
        (review.approved_by.as_slice(), review.missing.as_deref()),
        (&["bob".to_owned()][..], Some("1 of 2 approvals"))
    );
    // The agent of the owner speaks for the owner: it does not count either.
    let own = repo
        .give_verdict(lane, VerdictKind::Approved, None, Some(Author::agent("alice")), person("ada"))
        .unwrap();
    assert_eq!(own.missing.as_deref(), Some("1 of 2 approvals"));
    let review = approve(&mut repo, lane, "carol");
    assert_eq!(review.missing, None);
}

#[test]
fn what_lands_on_a_permanent_lane_needs_what_that_lane_says() {
    let (_tmp, root, mut repo) = shop("");
    let config = root.join(".kito/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("{text}\n[lanes.staging]\nrequired = 1\n")).unwrap();
    repo.reload_config().unwrap();
    let staging: LaneId = repo.create_lane("staging", None).unwrap().id.parse().unwrap();

    let stack = Stack { on: Some(staging), target: None };
    let info = repo.open_lane_on("alice", None, None, "prices in cents", &["./".to_owned()], stack).unwrap();
    let (lane, work): (LaneId, PathBuf) = (info.id.parse().unwrap(), info.workdir.unwrap());
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", CENTS));
    repo.scan_lane(lane).unwrap();
    assert_eq!(repo.review(lane).unwrap().required, 1);
    assert_eq!(repo.integrate(lane).unwrap().missing.as_deref(), Some("0 of 1 approval"));
    approve(&mut repo, lane, "bob");
    assert_eq!(repo.integrate(lane).unwrap().result, Outcome::Integrated);

    // A lane that lands on main needs none here.
    let (other, _) = lane_in_cents(&mut repo);
    assert_eq!(repo.review(other).unwrap().required, 0);
}
