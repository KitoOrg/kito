//! The swarm simulator on a small swarm: no fake conflict, every real one found.

use kitod::client::Client;
use kitod::simulate::{project, run, Plan};
use kitod::Repo;
use serde_json::{json, Value};
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn six_agents_four_rounds_and_the_three_real_conflicts() {
    std::env::set_var("KITOD_BIN", env!("CARGO_BIN_EXE_kitod"));
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    Repo::init(&root).unwrap().close().unwrap();
    let plan = Plan { agents: 6, rounds: 4, real_conflicts: true, snapshots: None };
    fs::write(root.join(".gitignore"), ".kito/\n").unwrap();
    for (path, content) in project(plan.agents) {
        fs::create_dir_all(root.join(&path).parent().unwrap()).unwrap();
        fs::write(root.join(&path), content).unwrap();
    }
    // Every integration is verified: nothing half-merged may reach the main lane.
    fs::write(
        root.join(".kito/config.toml"),
        "[author]\nname = \"ada\"\n[verify]\ncommands = [\"! grep -rnE '^(<<<<<<<|=======|>>>>>>>)' src\", \"test -f src/types.ts\"]\n",
    )
    .unwrap();
    let mut client = Client::connect_or_start(&root).unwrap();
    let _: Value = client.call("checkpoint", json!({ "label": "start" })).unwrap();

    let report = run(&root, &plan).unwrap();
    println!("{}", report.to_markdown());
    assert!(report.fake_conflicts.is_empty(), "fake conflicts: {:#?}", report.fake_conflicts);
    assert!(report.problems.is_empty(), "{:#?}", report.problems);
    assert_eq!((report.integrations, report.integrated), (24, 24));
    assert_eq!(report.real_conflicts_found, ["rename_rename", "remove_modify", "body_body"]);
    assert_eq!(report.verification_failures, 0);
    assert!(report.is_clean());
    // One checkpoint per integration that brought something, plus the start.
    assert!(report.checkpoints >= 1 + 24 + 3, "{}", report.checkpoints);

    // Git has the same files, commit by commit, each by its agent.
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(&root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["fsck", "--strict", "--no-dangling"]);
    assert_eq!(git(&["status", "--porcelain"]), "");
    let authors = git(&["log", "--format=%an"]);
    for k in 0..plan.agents {
        assert!(authors.lines().filter(|a| *a == format!("agent-{k}")).count() >= 4, "{authors}");
    }
    assert_eq!(
        git(&["show", "HEAD:src/shared/math.ts"]),
        fs::read_to_string(root.join("src/shared/math.ts")).unwrap()
    );

    let _: Value = client.call("shutdown", json!({})).unwrap();
    let socket = kitod::rpc::socket_path(&root);
    let deadline = Instant::now() + Duration::from_secs(20);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(Repo::open(&root).unwrap().store().verify().unwrap().is_ok());
}
