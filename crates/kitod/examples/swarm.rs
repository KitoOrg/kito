//! The phase 3 demo: a swarm of simulated agents on a TypeScript project.
//!
//! ```text
//! cargo build --release -p kitod
//! cargo run --release -p kitod --example swarm -- --agents 10 --rounds 4
//! ```
//!
//! Every agent works in its own lane, all at the same time and on the same
//! files; every integration is verified (with `tsc` if it is installed)
//! before it reaches the main lane. The run fails if a single conflict is
//! found where the script has none, or if a staged real conflict is missed.
//!
//! `--verify CMD` sets the verification command, `--keep` leaves the
//! repository on disk for a look.
//!
//! `--compare-git` gives git the same work afterwards: for every
//! integration of the rounds, the three states Kito merged (where the lane
//! started, the main lane as it was, the lane's working copy) go through
//! `git merge-tree`, which merges as `git merge` does, and the outcome is
//! counted: a conflict, the same result, or another one.

use kitod::client::Client;
use kitod::simulate::{project, run, Plan};
use kitod::Repo;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").current_dir(root).args(args).output().map_err(|e| e.to_string())?;
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        false => Err(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr))),
    }
}

/// Runs git with an index of its own, on a working tree given by the caller.
fn git_on(repo: &Path, tree: &Path, index: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(repo)
        .env("GIT_WORK_TREE", tree)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned()),
        false => Err(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr))),
    }
}

/// The same integrations, merged by git. For each one git is given what
/// Kito had: the state the lane started from (the commit of its previous
/// integration, or the first one), the main lane as it was just before
/// (the parent of the commit Kito made), and the lane's working copy. The
/// merge is `git merge-tree --write-tree`, the merge of `git merge`
/// without a working tree. Where git stops, the work goes on from Kito's
/// result, as if someone had settled the conflict that way.
fn with_git(root: &Path, snapshots: &Path, records: &[kitod::simulate::Record]) -> Result<String, String> {
    let scratch = snapshots.join("git");
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let repo = scratch.join("repo");
    git(root, &["clone", "-q", "--no-checkout", ".", &repo.to_string_lossy()])?;
    let first = git(&repo, &["rev-list", "--max-parents=0", "HEAD"])?.trim().to_owned();
    let identity = ["-c", "user.name=swarm", "-c", "user.email=swarm@example.com"];

    let (mut same, mut other, mut conflicts) = (0usize, Vec::new(), Vec::new());
    let mut files_in_conflict: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for record in records {
        let Some(kito) = &record.git_commit else {
            return Err(format!("agent-{} round {}: no git commit", record.agent, record.round));
        };
        let name = format!("agent-{} round {}", record.agent, record.round);
        // Where the lane started: its own previous integration.
        let base = match record.round {
            0 => first.clone(),
            round => records
                .iter()
                .find(|r| r.agent == record.agent && r.round + 1 == round)
                .and_then(|r| r.git_commit.clone())
                .ok_or_else(|| format!("{name}: its previous integration is missing"))?,
        };
        let ours = git(&repo, &["rev-parse", &format!("{kito}^")])?.trim().to_owned();
        // The lane's working copy, as a commit on top of where it started.
        let lane = snapshots.join(format!("agent-{}/round-{}", record.agent, record.round));
        let index = scratch.join("index");
        let _ = std::fs::remove_file(&index);
        git_on(&repo, &lane, &index, &["add", "-A", "."])?;
        let tree = git_on(&repo, &lane, &index, &["write-tree"])?;
        let mut commit = identity.to_vec();
        commit.extend(["commit-tree", &tree, "-p", &base, "-m", &name]);
        let theirs = git(&repo, &commit)?.trim().to_owned();

        let merged = Command::new("git")
            .current_dir(&repo)
            .args(["merge-tree", "--write-tree", "--name-only", "--no-messages"])
            .arg(format!("--merge-base={base}"))
            .args([&ours, &theirs])
            .output()
            .map_err(|e| e.to_string())?;
        let out = String::from_utf8_lossy(&merged.stdout).into_owned();
        let mut lines = out.lines();
        let result = lines.next().unwrap_or_default().to_owned();
        match merged.status.code() {
            Some(0) => {
                let expected = git(&repo, &["rev-parse", &format!("{kito}^{{tree}}")])?.trim().to_owned();
                match result == expected {
                    true => same += 1,
                    false => other.push(name),
                }
            }
            Some(1) => {
                let files: Vec<&str> = lines.filter(|line| !line.is_empty()).collect();
                for file in &files {
                    *files_in_conflict.entry((*file).to_owned()).or_default() += 1;
                }
                conflicts.push(format!("{name}: {}", files.join(", ")));
            }
            _ => return Err(format!("{name}: git merge-tree: {}", String::from_utf8_lossy(&merged.stderr))),
        }
    }

    let total = records.len();
    let mut text = format!(
        "## The same {total} integrations, merged by git\n\n| | git | Kito |\n| --- | --- | --- |\n\
         | Merged with no conflict | {} | {total} |\n| Stopped on a conflict | {} | 0 |\n\
         | Merged to the same files as Kito | {same} | |\n| Merged to other files than Kito | {} | |\n",
        same + other.len(),
        conflicts.len(),
        other.len(),
    );
    if !files_in_conflict.is_empty() {
        text.push_str("\nFiles git stopped on, and how many times:\n");
        for (file, times) in &files_in_conflict {
            text.push_str(&format!("- {file}: {times}\n"));
        }
    }
    for name in &other {
        text.push_str(&format!("\ngit merged {name} without a conflict, to other files than Kito.\n"));
    }
    Ok(text)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let number = |name: &str, default: usize| value(name).and_then(|v| v.parse().ok()).unwrap_or(default);
    let compare_git = args.iter().any(|a| a == "--compare-git");
    let snapshots = std::env::temp_dir().join(format!("kito-swarm-{}-lanes", std::process::id()));
    let _ = std::fs::remove_dir_all(&snapshots);
    let plan = Plan {
        agents: number("--agents", 10),
        rounds: number("--rounds", 4),
        real_conflicts: true,
        snapshots: compare_git.then(|| snapshots.clone()),
    };
    let keep = args.iter().any(|a| a == "--keep");

    if std::env::var_os("KITOD_BIN").is_none() {
        // target/<profile>/examples/swarm → target/<profile>/kitod
        let exe = std::env::current_exe()?;
        let candidate: PathBuf = exe
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join(format!("kitod{}", std::env::consts::EXE_SUFFIX)))
            .unwrap_or_default();
        if !candidate.is_file() {
            return Err("kitod not found: run `cargo build --release -p kitod` first or set KITOD_BIN".into());
        }
        std::env::set_var("KITOD_BIN", candidate);
    }
    let has_tsc = Command::new("tsc").arg("--version").output().is_ok_and(|out| out.status.success());
    let verify = value("--verify").unwrap_or_else(|| match has_tsc {
        true => "tsc --noEmit -p .".to_owned(),
        false => "! grep -rnE '^(<<<<<<<|=======|>>>>>>>)' src".to_owned(),
    });

    let root = std::env::temp_dir().join(format!("kito-swarm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    Repo::init(&root)?.close()?;
    std::fs::write(root.join(".gitignore"), ".kito/\n")?;
    for (path, content) in project(plan.agents) {
        std::fs::create_dir_all(root.join(&path).parent().ok_or("a path without a directory")?)?;
        std::fs::write(root.join(&path), content)?;
    }
    std::fs::write(
        root.join(".kito/config.toml"),
        format!(
            "[author]\nname = \"ada\"\nemail = \"ada@example.com\"\n[verify]\ncommands = [{}]\n",
            json!(verify)
        ),
    )?;
    let mut client = Client::connect_or_start(&root)?;
    let _: Value = client.call("checkpoint", json!({ "label": "the project, before the swarm" }))?;

    println!(
        "{} agents, {} rounds each, on a TypeScript project of {} files in {}",
        plan.agents,
        plan.rounds,
        project(plan.agents).len(),
        root.display()
    );
    println!("Verification before every integration: `{verify}`\n");
    let started = Instant::now();
    let report = run(&root, &plan)?;
    let took = started.elapsed();
    println!("{}", report.to_markdown());
    println!("Wall time: {:.1} s\n", took.as_secs_f64());

    // What git sees of it: one commit per integration, each by its agent.
    let log = git(&root, &["log", "--format=%an"])?;
    let mut by_author: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    log.lines().for_each(|author| *by_author.entry(author).or_default() += 1);
    println!("git log: {} commits ({})", log.lines().count(), {
        let parts: Vec<String> = by_author.iter().map(|(author, n)| format!("{author} {n}")).collect();
        parts.join(", ")
    });
    git(&root, &["fsck", "--strict", "--no-dangling"])?;
    println!(
        "git fsck --strict: ok;  git status: {}",
        match git(&root, &["status", "--porcelain"])?.trim() {
            "" => "clean",
            other => other,
        }
    );
    if has_tsc {
        let checked = Command::new("tsc").args(["--noEmit", "-p", "."]).current_dir(&root).output()?;
        println!(
            "tsc on the main lane after the swarm: {}",
            if checked.status.success() { "no errors" } else { "ERRORS" }
        );
        if !checked.status.success() {
            println!("{}", String::from_utf8_lossy(&checked.stdout));
        }
    }

    if compare_git {
        println!("\n{}", with_git(&root, &snapshots, &report.records)?);
        let _ = std::fs::remove_dir_all(&snapshots);
    }

    let _: Value = client.call("shutdown", json!({}))?;
    let socket = kitod::rpc::socket_path(&root);
    let deadline = Instant::now() + Duration::from_secs(30);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let verified = Repo::open(&root)?.store().verify()?;
    println!(
        "store: {} ops, {} blobs, {}",
        verified.ops,
        verified.blobs,
        match verified.is_ok() {
            true => "verified".to_owned(),
            false => format!("PROBLEMS: {:?}", verified.problems),
        }
    );
    if keep {
        println!("The repository is in {}", root.display());
    } else {
        let _ = std::fs::remove_dir_all(&root);
    }
    if !report.is_clean()
        || !verified.is_ok()
        || report.real_conflicts_found.len() != report.real_conflicts_staged
    {
        return Err("the swarm did not go as scripted".into());
    }
    println!("\nNo fake conflicts.");
    Ok(())
}
