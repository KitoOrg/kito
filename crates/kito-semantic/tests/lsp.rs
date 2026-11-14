//! The provider that asks a language server: with a server made for the
//! tests, and with the real ones for TypeScript and Go where they are
//! installed.

mod common;

use common::World;
use kito_core::{Lang, RefSource};
use kito_semantic::{Error, LspConfig, LspProvider, Semantic, SemanticProvider};
use std::path::{Path, PathBuf};
use std::process::Command;

const AUTH: &str = "export function getUser(id: string): string {\n  return id;\n}\n\nexport function current(): string {\n  return getUser(\"me\");\n}\n";
const WEB: &str =
    "import { getUser } from \"./auth\";\n\nexport function profile(id: string): string {\n  return getUser(id);\n}\n";

fn fake_server() -> String {
    // target/<profile>/deps/lsp-… → target/<profile>/examples/fake_lsp
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().unwrap().parent().unwrap().join("examples/fake_lsp");
    assert!(
        path.is_file(),
        "{} is not built: `cargo test -p kito-semantic` builds it, a single `--test` does not (then: `cargo build -p kito-semantic --example fake_lsp`)",
        path.display()
    );
    path.to_str().unwrap().to_owned()
}

fn fake(root: &Path, timeout_secs: u64) -> LspProvider {
    let config = LspConfig {
        command: vec![fake_server()],
        languages: vec![Lang::TypeScript],
        timeout_secs,
        options: None,
    };
    LspProvider::new("fake", config, root)
}

/// What the server of the tests was told. Notifications are not answered,
/// so the last ones may still be on their way: it waits until every
/// document opened has been closed.
fn log(root: &Path) -> Vec<String> {
    let count = |lines: &[String], what: &str| lines.iter().filter(|line| line.starts_with(what)).count();
    let mut lines = Vec::new();
    for _ in 0..100 {
        let text = std::fs::read_to_string(root.join(".fake-lsp-log")).unwrap_or_default();
        lines = text.lines().map(|line| line.trim().to_owned()).collect();
        if count(&lines, "textDocument/didOpen") == count(&lines, "textDocument/didClose") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    lines
}

#[tokio::test]
async fn a_lane_is_shown_to_the_server_as_open_documents() {
    let tmp = tempfile::tempdir().unwrap();
    let main = World::on_disk(
        tmp.path(),
        &[
            ("src/auth.ts", AUTH),
            ("src/web.ts", WEB),
            ("src/old.ts", "import { getUser } from \"./auth\";\nexport const old = getUser(\"old\");\n"),
        ],
    );
    let server = fake(tmp.path(), 20);
    let get_user = main.id("src/auth.ts", "getUser");

    let found = server.usages(&main.snapshot(), get_user).await.unwrap();
    assert_eq!(
        main.show(&found),
        [
            "src/auth.ts:6 `return getUser(\"me\");` 1.0",
            "src/old.ts:1 `import { getUser } from \"./auth\";` 1.0",
            "src/old.ts:2 `export const old = getUser(\"old\");` 1.0",
            "src/web.ts:1 `import { getUser } from \"./auth\";` 1.0",
            "src/web.ts:4 `return getUser(id);` 1.0",
        ]
    );
    assert!(found.iter().all(|usage| usage.source == RefSource::Lsp("fake".into())));

    // A lane: one file edited, one new, one removed. Nothing on disk changes.
    let lane = main.with(&[
        ("src/web.ts", Some(&WEB.replace("return getUser(id);", "return getUser(id) + getUser(\"twice\");"))),
        (
            "src/new.ts",
            Some("import { getUser } from \"./auth\";\n\nexport const fresh = getUser(\"new\");\n"),
        ),
        ("src/old.ts", None),
    ]);
    let found = server.usages(&lane.snapshot(), lane.id("src/auth.ts", "getUser")).await.unwrap();
    assert_eq!(
        lane.show(&found),
        [
            "src/auth.ts:6 `return getUser(\"me\");` 1.0",
            "src/new.ts:1 `import { getUser } from \"./auth\";` 1.0",
            "src/new.ts:3 `export const fresh = getUser(\"new\");` 1.0",
            "src/web.ts:1 `import { getUser } from \"./auth\";` 1.0",
            "src/web.ts:4 `return getUser(id) + getUser(\"twice\");` 1.0",
            "src/web.ts:4 `return getUser(id) + getUser(\"twice\");` 1.0",
        ]
    );
    assert_eq!(std::fs::read_to_string(tmp.path().join("src/web.ts")).unwrap(), WEB);
    assert!(!tmp.path().join("src/new.ts").exists());

    // Back on the main lane, the server has forgotten the lane.
    let again = server.usages(&main.snapshot(), get_user).await.unwrap();
    assert_eq!(main.show(&again).len(), 5);

    // One server for the three questions; every document opened was closed.
    let log = log(tmp.path());
    assert_eq!(log.iter().filter(|line| *line == "initialized").count(), 1);
    let opened = log.iter().filter(|line| line.starts_with("textDocument/didOpen")).count();
    let closed = log.iter().filter(|line| line.starts_with("textDocument/didClose")).count();
    assert_eq!((opened, closed), (6, 6), "{log:#?}");
    assert!(log.contains(&"textDocument/didOpen src/new.ts".to_owned()));

    // The references that leave a file, one question per name.
    let refs = server.references(&main.snapshot(), Path::new("src/web.ts")).await.unwrap();
    let shown: Vec<String> = refs
        .iter()
        .map(|r| {
            format!(
                "{} -> {}",
                main.table.qualified_name(r.from).unwrap(),
                main.table.qualified_name(r.to).unwrap()
            )
        })
        .collect();
    assert_eq!(shown, ["profile -> getUser"]);
    server.shutdown().await;
}

#[tokio::test]
async fn a_server_that_fails_is_stopped_and_the_heuristic_answers() {
    let tmp = tempfile::tempdir().unwrap();
    let main = World::on_disk(tmp.path(), &[("src/auth.ts", AUTH), ("src/web.ts", WEB)]);
    let get_user = main.id("src/auth.ts", "getUser");
    let server = fake(tmp.path(), 1);

    // It never answers: the question ends when its time is up.
    std::fs::write(tmp.path().join(".fake-lsp-mode"), "hang").unwrap();
    let started = std::time::Instant::now();
    let error = server.usages(&main.snapshot(), get_user).await.unwrap_err();
    assert!(matches!(error, Error::Timeout { seconds: 1, .. }), "{error}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    // It dies in the middle of a question.
    std::fs::write(tmp.path().join(".fake-lsp-mode"), "crash").unwrap();
    let error = server.usages(&main.snapshot(), get_user).await.unwrap_err();
    assert!(matches!(error, Error::Server { .. }), "{error}");

    // Each time it was stopped, and the next question started it again.
    std::fs::write(tmp.path().join(".fake-lsp-mode"), "").unwrap();
    assert_eq!(server.usages(&main.snapshot(), get_user).await.unwrap().len(), 3);
    assert_eq!(log(tmp.path()).iter().filter(|line| *line == "initialized").count(), 3);

    // Among the providers of a repository, a server that fails gives way
    // to the heuristic, which is less sure, and the failure is reported.
    std::fs::write(tmp.path().join(".fake-lsp-mode"), "crash").unwrap();
    let missing = LspConfig {
        command: vec!["kito-no-such-server".into()],
        languages: vec![Lang::Go],
        timeout_secs: 5,
        options: None,
    };
    let semantic = Semantic::new(vec![server, LspProvider::new("gone", missing, tmp.path())]);
    assert!(
        semantic.has_server(Lang::TypeScript)
            && semantic.has_server(Lang::Go)
            && !semantic.has_server(Lang::Python)
    );
    let mut problems = Vec::new();
    let found = semantic.usages(&main.snapshot(), get_user, &mut problems).await.unwrap();
    assert_eq!(found.len(), 3);
    assert!(found
        .iter()
        .all(|usage| usage.source == RefSource::TreeSitterHeuristic && usage.confidence < 0.9));
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("fake"), "{problems:?}");

    // A server that is not installed is found out once, and then left alone.
    let go = World::on_disk(
        tmp.path(),
        &[("users/users.go", "package users\n\nfunc GetUser() {}\n\nfunc Two() {\n\tGetUser()\n}\n")],
    );
    let mut problems = Vec::new();
    for _ in 0..2 {
        let found =
            semantic.usages(&go.snapshot(), go.id("users/users.go", "GetUser"), &mut problems).await.unwrap();
        assert_eq!(found.len(), 1);
    }
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("not installed"), "{problems:?}");
    assert!(!semantic.has_server(Lang::Go));
    semantic.shutdown().await;
}

fn installed(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok_and(|out| out.status.success())
}

/// The TypeScript package the `tsc` on the path belongs to: the server
/// looks for one in the project, as a project that uses TypeScript has.
fn typescript_package() -> Option<PathBuf> {
    let out = Command::new("sh").args(["-c", "command -v tsc"]).output().ok()?;
    let tsc = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim()).canonicalize().ok()?;
    let package = tsc.parent()?.parent()?.to_owned();
    package.join("lib/tsserver.js").is_file().then_some(package)
}

#[tokio::test]
async fn the_typescript_server_follows_a_symbol_into_a_lane() {
    let Some(typescript) = typescript_package().filter(|_| installed("typescript-language-server")) else {
        eprintln!("typescript-language-server or typescript is not installed: skipped");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
    std::os::unix::fs::symlink(typescript, tmp.path().join("node_modules/typescript")).unwrap();
    let main = World::on_disk(
        tmp.path(),
        &[
            ("tsconfig.json", "{ \"compilerOptions\": { \"strict\": true, \"module\": \"esnext\", \"moduleResolution\": \"bundler\" }, \"include\": [\"src\"] }\n"),
            ("src/auth.ts", AUTH),
            ("src/web.ts", WEB),
            // Same name, another thing: a language server is not fooled.
            ("src/other.ts", "function getUser(n: number): number {\n  return n;\n}\n\nexport const first = getUser(1);\n"),
        ],
    );
    let config = LspConfig {
        command: vec!["typescript-language-server".into(), "--stdio".into()],
        languages: vec![Lang::TypeScript, Lang::JavaScript],
        timeout_secs: 120,
        options: None,
    };
    let server = LspProvider::new("typescript-language-server", config, tmp.path());
    let get_user = main.id("src/auth.ts", "getUser");
    let found = server.usages(&main.snapshot(), get_user).await.unwrap();
    assert_eq!(
        main.show(&found),
        [
            "src/auth.ts:6 `return getUser(\"me\");` 1.0",
            "src/web.ts:1 `import { getUser } from \"./auth\";` 1.0",
            "src/web.ts:4 `return getUser(id);` 1.0",
        ]
    );

    // In a lane: a new use in a file that exists, and one in a new file.
    let lane = main.with(&[
        ("src/web.ts", Some(&WEB.replace("return getUser(id);", "return getUser(id) + getUser(\"twice\");"))),
        (
            "src/fresh.ts",
            Some("import { getUser } from \"./auth\";\n\nexport const fresh = getUser(\"new\");\n"),
        ),
    ]);
    let found = server.usages(&lane.snapshot(), lane.id("src/auth.ts", "getUser")).await.unwrap();
    assert_eq!(
        lane.show(&found),
        [
            "src/auth.ts:6 `return getUser(\"me\");` 1.0",
            "src/fresh.ts:1 `import { getUser } from \"./auth\";` 1.0",
            "src/fresh.ts:3 `export const fresh = getUser(\"new\");` 1.0",
            "src/web.ts:1 `import { getUser } from \"./auth\";` 1.0",
            "src/web.ts:4 `return getUser(id) + getUser(\"twice\");` 1.0",
            "src/web.ts:4 `return getUser(id) + getUser(\"twice\");` 1.0",
        ]
    );
    // And back.
    assert_eq!(server.usages(&main.snapshot(), get_user).await.unwrap().len(), 3);

    let refs = server.references(&main.snapshot(), Path::new("src/web.ts")).await.unwrap();
    let to: Vec<PathBuf> = refs.iter().map(|r| main.table.get(r.to).unwrap().file.clone()).collect();
    assert_eq!(to, [PathBuf::from("src/auth.ts")]);
    server.shutdown().await;
}

#[tokio::test]
async fn the_go_server_follows_a_symbol_into_a_lane() {
    if !installed("gopls") || !installed("go") {
        eprintln!("gopls is not installed: skipped");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let web = "package web\n\nimport \"app/users\"\n\nfunc Profile(id string) string {\n\treturn users.GetUser(id)\n}\n";
    let main = World::on_disk(
        tmp.path(),
        &[
            ("go.mod", "module app\n\ngo 1.21\n"),
            ("users/users.go", "package users\n\nfunc GetUser(id string) string {\n\treturn id\n}\n\nfunc Twice(id string) string {\n\treturn GetUser(GetUser(id))\n}\n"),
            ("web/web.go", web),
            ("web/local.go", "package web\n\nfunc GetUser(id string) string {\n\treturn \"local \" + id\n}\n\nfunc Local(id string) string {\n\treturn GetUser(id)\n}\n"),
        ],
    );
    let config = LspConfig {
        command: vec!["gopls".into()],
        languages: vec![Lang::Go],
        timeout_secs: 120,
        options: None,
    };
    let server = LspProvider::new("gopls", config, tmp.path());
    let get_user = main.id("users/users.go", "GetUser");
    let found = server.usages(&main.snapshot(), get_user).await.unwrap();
    assert_eq!(
        main.show(&found),
        [
            "users/users.go:8 `return GetUser(GetUser(id))` 1.0",
            "users/users.go:8 `return GetUser(GetUser(id))` 1.0",
            "web/web.go:6 `return users.GetUser(id)` 1.0",
        ]
    );
    let lane = main.with(&[(
        "web/web.go",
        Some(&web.replace("return users.GetUser(id)", "return users.GetUser(id) + users.GetUser(\"twice\")")),
    )]);
    let found = server.usages(&lane.snapshot(), lane.id("users/users.go", "GetUser")).await.unwrap();
    assert_eq!(
        found.iter().filter(|usage| usage.file == Path::new("web/web.go")).count(),
        2,
        "{:?}",
        lane.show(&found)
    );
    assert_eq!(server.usages(&main.snapshot(), get_user).await.unwrap().len(), 3);
    server.shutdown().await;
}
