//! Review of a lane (ADR 0004): comments, and telling the agent.

use kito_core::paths::Canonical as _;
use kito_core::*;
use kitod::comments::CommentInfo;
use kitod::Repo;
use std::fs;
use std::path::{Path, PathBuf};

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
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
    let root = tmp.path().canonical().unwrap();
    drop(Repo::init(&root).unwrap());
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.ts", SHOP);
    write(&root, "README.md", "# Shop\n");
    repo.checkpoint("start").unwrap();
    (tmp, root, repo)
}

fn open(repo: &mut Repo, agent: &str, intent: &str) -> (LaneId, PathBuf) {
    let info = repo.open_lane(agent, None, None, intent, &["./".to_owned()]).unwrap();
    (info.id.parse().unwrap(), info.workdir.unwrap())
}

fn bob() -> Option<Author> {
    Some(Author::human("bob"))
}

/// A line per comment: who, on what, what, and its marks.
fn shown(comments: &[CommentInfo]) -> Vec<String> {
    comments
        .iter()
        .map(|comment| {
            let on = comment.on.as_ref().map(|on| format!(" on {on}")).unwrap_or_default();
            let to = comment.reply_to.as_ref().map(|to| format!(" to {to}")).unwrap_or_default();
            let old = if comment.outdated { " [outdated]" } else { "" };
            let done =
                comment.resolved_by.as_ref().map(|by| format!(" [resolved by {by}]")).unwrap_or_default();
            format!("{}{on}{to} ({}): {}{old}{done}", comment.id, comment.by, comment.text)
        })
        .collect()
}

#[test]
fn a_lane_is_commented_answered_and_the_comments_resolved() {
    let (_tmp, root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", "prices that are never negative");
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.max(price, 0);"));

    // On the lane, on a function, on lines, on a file: as the lane has
    // them now, saved a moment ago.
    let whole = repo.add_comment(lane, None, "Good, but see below.", bob(), None, None).unwrap();
    assert_eq!((whole.id.as_str(), whole.lane.as_str(), whole.on.as_deref()), ("comment#1", "lane/1", None));
    let lines = repo
        .add_comment(lane, Some("src/shop.ts:4"), "And when a price is not a number?", bob(), None, None)
        .unwrap();
    assert_eq!(lines.on.as_deref(), Some("src/shop.ts:4"));
    assert_eq!(lines.quoted.as_deref(), Some("    sum += Math.max(price, 0);"));
    assert_eq!(lines.lines, Some((4, 4)));
    repo.add_comment(
        lane,
        Some("src/shop.ts:discount"),
        "Out of this lane's way: leave it.",
        bob(),
        None,
        None,
    )
    .unwrap();
    repo.add_comment(lane, Some("README.md"), "Say that prices are never negative.", bob(), None, None)
        .unwrap();

    // The agent answers one; an answer to an answer joins the same exchange.
    let alice = Some(Author::agent("alice"));
    let answer = repo
        .add_comment(
            lane,
            None,
            "Then it is counted as 0.",
            alice.clone(),
            Some(Author::human("ada")),
            Some(CommentId(2)),
        )
        .unwrap();
    assert_eq!((answer.reply_to.as_deref(), answer.person.as_deref()), (Some("comment#2"), Some("ada")));
    repo.add_comment(lane, None, "Say so in a test.", bob(), None, Some(CommentId(5))).unwrap();

    let said = repo.comments(lane, false).unwrap();
    assert_eq!(
        shown(&said),
        [
            "comment#1 (bob): Good, but see below.",
            "comment#2 on src/shop.ts:4 (bob): And when a price is not a number?",
            "comment#5 to comment#2 (alice): Then it is counted as 0.",
            "comment#6 to comment#2 (bob): Say so in a test.",
            "comment#3 on src/shop.ts:discount (bob): Out of this lane's way: leave it.",
            "comment#4 on README.md (bob): Say that prices are never negative.",
        ]
    );

    // Resolved, a comment is kept and no longer told, with its answers.
    let done = repo.resolve_comment(CommentId(2), None, None, true).unwrap();
    assert_eq!(done.resolved_by.as_deref(), Some("ada"));
    repo.resolve_comment(CommentId(1), bob(), None, true).unwrap();
    assert_eq!(repo.comments(lane, false).unwrap().len(), 2);
    let all = shown(&repo.comments(lane, true).unwrap());
    assert_eq!(all.len(), 6);
    assert_eq!(all[0], "comment#1 (bob): Good, but see below. [resolved by bob]");

    // They are in the store: the next daemon has them.
    drop(repo);
    let mut repo = Repo::open(&root).unwrap();
    assert_eq!(shown(&repo.comments(lane, true).unwrap()), all);

    // What is refused, and why.
    let refused = |result: kitod::Result<CommentInfo>, words: &str| {
        let said = result.unwrap_err().to_string();
        assert!(said.contains(words), "{said}");
    };
    refused(repo.add_comment(LaneId::MAIN, None, "On main.", None, None, None), "a comment is on a lane");
    refused(repo.add_comment(lane, None, "?", None, None, None), "needs a few words");
    refused(repo.add_comment(lane, Some("src/none.ts"), "Where?", None, None, None), "no such file");
    refused(
        repo.add_comment(lane, Some("src/shop.ts:99"), "Where?", None, None, None),
        "there is no line 99",
    );
    refused(
        repo.add_comment(lane, Some("src/shop.ts:tax"), "Where?", None, None, None),
        "nothing called `tax`",
    );
    refused(repo.add_comment(lane, None, "Again.", None, None, Some(CommentId(2))), "comment#2 is resolved");
    refused(repo.add_comment(lane, None, "Again.", None, None, Some(CommentId(9))), "no such comment");
    refused(
        repo.add_comment(lane, Some("README.md"), "There.", None, None, Some(CommentId(3))),
        "leave the target out",
    );
    refused(repo.resolve_comment(CommentId(2), None, None, true), "resolved already");
    refused(repo.resolve_comment(CommentId(5), None, None, true), "resolve comment#2");
}

#[test]
fn a_comment_stays_on_its_lines_while_the_lane_has_them() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", "prices that are never negative");
    repo.add_comment(lane, Some("src/shop.ts:3-4"), "Skip the negative ones.", bob(), None, None).unwrap();
    repo.add_comment(lane, Some("src/shop.ts:discount"), "Round half up.", bob(), None, None).unwrap();
    repo.add_comment(lane, Some("README.md"), "Too short.", bob(), None, None).unwrap();
    let on = |repo: &Repo| -> Vec<(Option<String>, bool)> {
        repo.comments(lane, false)
            .unwrap()
            .into_iter()
            .map(|comment| (comment.on, comment.outdated))
            .collect()
    };
    let at = |on: &str, outdated: bool| (Some(on.to_owned()), outdated);
    assert_eq!(
        on(&repo),
        [at("src/shop.ts:3-4", false), at("src/shop.ts:discount", false), at("README.md", false)]
    );

    // Lines are added above: the comment is where its lines went.
    write(&work, "src/shop.ts", &format!("// Prices are in euros.\n\n{SHOP}"));
    repo.scan_lane(lane).unwrap();
    assert_eq!(on(&repo)[0], at("src/shop.ts:5-6", false));
    assert_eq!(on(&repo)[1], at("src/shop.ts:discount", false), "moved, not changed");

    // The function is renamed: the comment is on it under its new name.
    repo.rename_symbol_with(lane, "src/shop.ts:discount", "rebate", None).unwrap();
    assert_eq!(on(&repo)[1], at("src/shop.ts:rebate", true));

    // The lines are rewritten, the file removed: the comments stay, with
    // what they were about.
    let now = fs::read_to_string(work.join("src/shop.ts")).unwrap();
    write(&work, "src/shop.ts", &now.replace("sum += price;", "sum += Math.max(price, 0);"));
    fs::remove_file(work.join("README.md")).unwrap();
    repo.scan_lane(lane).unwrap();
    let said = repo.comments(lane, false).unwrap();
    assert_eq!((said[0].on.as_deref(), said[0].outdated, said[0].lines), (Some("src/shop.ts"), true, None));
    assert_eq!(said[0].quoted.as_deref(), Some("  for (const price of prices) {\n    sum += price;"));
    assert_eq!((said[2].on.as_deref(), said[2].outdated), (Some("README.md"), true));
}

#[test]
fn the_agent_of_a_lane_is_told_the_comments_before_its_next_edit() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, _work) = open(&mut repo, "alice", "prices that are never negative");
    let file = Path::new("src/shop.ts");
    // Told the reasons of the code first, so that only comments are left to tell.
    while !repo.before_edit(Some(lane), file, &[(4, 4)], false).unwrap().allowed {}

    repo.add_comment(lane, Some("src/shop.ts:4"), "Skip the negative ones.", bob(), None, None).unwrap();
    // What the agent says itself it is not told back.
    repo.add_comment(lane, None, "Will do.", Some(Author::agent("alice")), None, Some(CommentId(1))).unwrap();

    let first = repo.before_edit(Some(lane), file, &[(4, 4)], false).unwrap();
    assert!(!first.allowed && first.kind == "comments", "{first:?}");
    assert_eq!(first.comments.len(), 1);
    assert!(
        first.message.contains("- comment#1 on src/shop.ts:4 (bob): Skip the negative ones.")
            && first.message.contains("    >     sum += price;")
            && first.message.contains("it goes through"),
        "{}",
        first.message
    );
    // Told: the same edit goes through, and so does one elsewhere.
    assert!(repo.before_edit(Some(lane), file, &[(4, 4)], false).unwrap().allowed);

    // Another comment is told when it comes; a resolved one never.
    repo.add_comment(lane, None, "And add a test.", bob(), None, None).unwrap();
    repo.add_comment(lane, None, "Never mind this one.", bob(), None, None).unwrap();
    repo.resolve_comment(CommentId(4), bob(), None, true).unwrap();
    let next = repo.before_edit(Some(lane), Path::new("README.md"), &[], true).unwrap();
    assert_eq!(next.kind, "comments");
    assert_eq!(next.comments.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["comment#3"]);

    // Another lane is told nothing of this one's.
    let (other, _) = open(&mut repo, "carol", "a readme that says more");
    let theirs = repo.before_edit(Some(other), Path::new("README.md"), &[], true).unwrap();
    assert_ne!(theirs.kind, "comments");
}

#[test]
fn a_comment_is_resolved_by_who_wrote_it_or_by_the_owner_of_the_lane() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, _work) = open(&mut repo, "alice", "prices that are never negative");
    repo.set_lane_owner(lane, Author::human("ada")).unwrap();
    for text in ["One thing.", "Another thing.", "A third thing."] {
        repo.add_comment(lane, None, text, bob(), None, None).unwrap();
    }
    // Somebody else may not, unless they may integrate.
    let said = repo.resolve_comment(CommentId(1), None, Some("carol"), false).unwrap_err().to_string();
    assert!(said.contains("resolved by who wrote it, by the owner of lane/1"), "{said}");
    assert!(repo.resolve_comment(CommentId(1), None, Some("carol"), true).is_ok());
    assert!(repo.resolve_comment(CommentId(2), bob(), Some("bob"), false).is_ok());
    assert!(repo.resolve_comment(CommentId(3), None, Some("ada"), false).is_ok());
    assert!(repo.comments(lane, false).unwrap().is_empty());
}

#[test]
fn a_review_suggests_lines_tells_the_agent_and_shows_what_changed_since() {
    let (_tmp, _root, mut repo) = shop();
    let (lane, work) = open(&mut repo, "alice", "prices that are never negative");
    write(&work, "src/shop.ts", &SHOP.replace("sum += price;", "sum += Math.floor(price);"));
    let file = Path::new("src/shop.ts");
    while !repo.before_edit(Some(lane), file, &[(4, 4)], false).unwrap().allowed {}

    // Bob suggests lines in the place of line 4, and asks for changes.
    let said =
        repo.add_comment(lane, Some("src/shop.ts:4"), "Round, do not floor.", bob(), None, None).unwrap();
    let said = repo.suggest(said.id.parse().unwrap(), "    sum += Math.round(price);\n").unwrap();
    assert_eq!(said.suggestion.as_deref(), Some("    sum += Math.round(price);"));
    repo.give_verdict(lane, VerdictKind::ChangesRequested, Some("Rounding."), bob(), None).unwrap();

    // The agent is told both, the lines suggested with the comment.
    let told = repo.before_edit(Some(lane), file, &[(4, 4)], false).unwrap();
    assert!(!told.allowed && told.kind == "comments", "{told:?}");
    assert!(
        told.message.contains("Round, do not floor.")
            && told.message.contains("    + ".to_owned().as_str())
            && told.message.contains("Math.round(price);")
            && told.message.contains("changes requested: Rounding."),
        "{}",
        told.message
    );

    // Nothing changed since bob's verdict; then the suggestion is applied.
    assert!(repo.changes_since(lane, "bob").unwrap().files.is_empty());
    let applied = repo.apply_suggestion(said.id.parse().unwrap(), bob(), None, true).unwrap();
    assert_eq!(applied.resolved_by.as_deref(), Some("bob"));
    let now = fs::read_to_string(work.join("src/shop.ts")).unwrap();
    assert!(now.contains("    sum += Math.round(price);\n") && !now.contains("floor"), "{now}");
    let since = repo.changes_since(lane, "bob").unwrap();
    assert_eq!(since.files.len(), 1);
    assert_eq!((since.files[0].added, since.files[0].removed), (1, 1));
    assert!(since.files[0].hunks[0].lines.iter().any(|line| line == "+    sum += Math.round(price);"));
    // Applied once: it is resolved, and suggests nothing more.
    assert!(repo.apply_suggestion(said.id.parse().unwrap(), bob(), None, true).is_err());
    // Who gave no verdict has nothing to compare with.
    assert!(repo.changes_since(lane, "carol").is_err());
}
