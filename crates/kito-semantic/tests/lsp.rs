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
    let found = Command::new("sh").args(["-c", &format!("command -v {program}")]).output();
    found.is_ok_and(|out| out.status.success())
}

/// A test of a real server is skipped where the server is not installed,
/// unless `KITO_TEST_REQUIRE_SERVERS` says it must run.
fn skip(what: &str) {
    assert!(std::env::var_os("KITO_TEST_REQUIRE_SERVERS").is_none(), "{what} is not installed");
    eprintln!("{what} is not installed: skipped");
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
        skip("typescript-language-server, or a TypeScript with a tsserver for it,");
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
        skip("gopls (or go)");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Not the temporary directory itself: its name starts with a dot, and
    // the Go tools do not look into directories named like that.
    let root = tmp.path().join("project");
    let web = "package web\n\nimport \"app/users\"\n\nfunc Profile(id string) string {\n\treturn users.GetUser(id)\n}\n";
    let main = World::on_disk(
        &root,
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
    let server = LspProvider::new("gopls", config, &root);
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

/// A real server, a symbol used in another file, and a lane that adds a use
/// in a file that exists and one in a new file: the uses the server must
/// find on the main lane, in the lane, and on the main lane again.
#[allow(clippy::too_many_arguments)]
async fn follows_into_a_lane(
    name: &str,
    command: &[&str],
    lang: Lang,
    files: &[(&str, &str)],
    symbol: (&str, &str),
    lane: &[(&str, Option<&str>)],
    on_main: &[&str],
    in_lane: &[&str],
    // What the project has that is not tracked: written once it is on disk.
    setup: &dyn Fn(&Path),
) {
    let tmp = tempfile::tempdir().unwrap();
    // Not the temporary directory itself, whose name starts with a dot.
    let root = tmp.path().join("project");
    let main = World::on_disk(&root, files);
    setup(&root);
    let config = LspConfig {
        command: command.iter().map(|part| (*part).to_owned()).collect(),
        languages: vec![lang],
        timeout_secs: 180,
        options: None,
    };
    let server = LspProvider::new(name, config, &root);
    let found = server.usages(&main.snapshot(), main.id(symbol.0, symbol.1)).await.unwrap();
    assert_eq!(main.show(&found), on_main, "on the main lane");
    assert!(found
        .iter()
        .all(|usage| usage.source == RefSource::Lsp(name.to_owned()) && usage.confidence == 1.0));

    let state = main.with(lane);
    let found = server.usages(&state.snapshot(), state.id(symbol.0, symbol.1)).await.unwrap();
    assert_eq!(state.show(&found), in_lane, "in the lane");

    let found = server.usages(&main.snapshot(), main.id(symbol.0, symbol.1)).await.unwrap();
    assert_eq!(main.show(&found), on_main, "back on the main lane");
    server.shutdown().await;
}

#[tokio::test]
async fn the_python_server_follows_a_symbol_into_a_lane() {
    if !installed("pyright-langserver") {
        skip("pyright-langserver");
        return;
    }
    let views = "from .models import get_user\n\n\ndef show(uid):\n    return get_user(uid)\n";
    follows_into_a_lane(
        "pyright",
        &["pyright-langserver", "--stdio"],
        Lang::Python,
        &[
            ("app/__init__.py", ""),
            ("app/models.py", "def get_user(uid):\n    return uid\n\n\ndef current():\n    return get_user(0)\n"),
            ("app/views.py", views),
            // Same name, another function.
            ("app/other.py", "def get_user(uid):\n    return None\n\n\ndef twice(uid):\n    return get_user(get_user(uid))\n"),
        ],
        ("app/models.py", "get_user"),
        &[
            ("app/views.py", Some(&views.replace("return get_user(uid)", "return get_user(uid) or get_user(0)"))),
            ("app/jobs.py", Some("from app.models import get_user\n\n\ndef run(uid):\n    return get_user(uid)\n")),
        ],
        &[
            "app/models.py:6 `return get_user(0)` 1.0",
            "app/views.py:1 `from .models import get_user` 1.0",
            "app/views.py:5 `return get_user(uid)` 1.0",
        ],
        &[
            "app/jobs.py:1 `from app.models import get_user` 1.0",
            "app/jobs.py:5 `return get_user(uid)` 1.0",
            "app/models.py:6 `return get_user(0)` 1.0",
            "app/views.py:1 `from .models import get_user` 1.0",
            "app/views.py:5 `return get_user(uid) or get_user(0)` 1.0",
            "app/views.py:5 `return get_user(uid) or get_user(0)` 1.0",
        ],
        &|_| {},
    )
    .await;
}

#[tokio::test]
async fn the_php_server_follows_a_symbol_into_a_lane() {
    if !installed("intelephense") {
        skip("intelephense");
        return;
    }
    let web = "<?php\nrequire_once __DIR__ . '/auth.php';\n\nfunction profile(string $id): string\n{\n    return get_user($id);\n}\n";
    follows_into_a_lane(
        "intelephense",
        &["intelephense", "--stdio"],
        Lang::Php,
        &[
            ("src/auth.php", "<?php\n\nfunction get_user(string $id): string\n{\n    return $id;\n}\n\nfunction current_user(): string\n{\n    return get_user('me');\n}\n"),
            ("src/web.php", web),
        ],
        ("src/auth.php", "get_user"),
        &[
            ("src/web.php", Some(&web.replace("return get_user($id);", "return get_user($id) . get_user('twice');"))),
            ("src/billing.php", Some("<?php\n\nfunction invoice(string $id): string\n{\n    return 'invoice for ' . get_user($id);\n}\n")),
        ],
        &["src/auth.php:10 `return get_user('me');` 1.0", "src/web.php:6 `return get_user($id);` 1.0"],
        &[
            "src/auth.php:10 `return get_user('me');` 1.0",
            "src/billing.php:5 `return 'invoice for ' . get_user($id);` 1.0",
            "src/web.php:6 `return get_user($id) . get_user('twice');` 1.0",
            "src/web.php:6 `return get_user($id) . get_user('twice');` 1.0",
        ],
        &|_| {},
    )
    .await;
}

#[tokio::test]
async fn the_c_server_follows_a_symbol_into_a_lane() {
    if !installed("clangd") {
        skip("clangd");
        return;
    }
    let web = "#include \"auth.h\"\n\nint profile(int id) {\n  return get_user(id);\n}\n";
    follows_into_a_lane(
        "clangd",
        &["clangd"],
        Lang::C,
        &[
            ("auth.h", "int get_user(int id);\n"),
            ("auth.c", "#include \"auth.h\"\n\nint get_user(int id) {\n  return id;\n}\n\nint current(void) {\n  return get_user(0);\n}\n"),
            ("web.c", web),
        ],
        ("auth.c", "get_user"),
        &[("web.c", Some(&web.replace("return get_user(id);", "return get_user(id) + get_user(2);")))],
        &["auth.c:8 `return get_user(0);` 1.0", "web.c:4 `return get_user(id);` 1.0"],
        &[
            "auth.c:8 `return get_user(0);` 1.0",
            "web.c:4 `return get_user(id) + get_user(2);` 1.0",
            "web.c:4 `return get_user(id) + get_user(2);` 1.0",
        ],
        // The list of what is compiled and how, as a build system writes
        // it: clangd looks for uses in the files it names.
        &|root| {
            let entry = |file: &str| {
                format!(
                    "{{ \"directory\": {:?}, \"file\": {file:?}, \"command\": \"cc -I. -c {file}\" }}",
                    root.display().to_string()
                )
            };
            let commands = format!("[{}, {}]\n", entry("auth.c"), entry("web.c"));
            std::fs::write(root.join("compile_commands.json"), commands).unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn the_rust_server_follows_a_symbol_into_a_lane() {
    if !installed("rust-analyzer") || !installed("cargo") {
        skip("rust-analyzer (or cargo)");
        return;
    }
    let web = "use crate::auth::get_user;\n\npub fn profile(id: u32) -> u32 {\n    get_user(id)\n}\n";
    follows_into_a_lane(
        "rust-analyzer",
        &["rust-analyzer"],
        Lang::Rust,
        &[
            ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            ("src/lib.rs", "pub mod auth;\npub mod web;\n"),
            ("src/auth.rs", "pub fn get_user(id: u32) -> u32 {\n    id\n}\n\npub fn current() -> u32 {\n    get_user(0)\n}\n"),
            ("src/web.rs", web),
        ],
        ("src/auth.rs", "get_user"),
        &[("src/web.rs", Some(&web.replace("    get_user(id)\n", "    get_user(id) + get_user(2)\n")))],
        &[
            "src/auth.rs:6 `get_user(0)` 1.0",
            "src/web.rs:1 `use crate::auth::get_user;` 1.0",
            "src/web.rs:4 `get_user(id)` 1.0",
        ],
        &[
            "src/auth.rs:6 `get_user(0)` 1.0",
            "src/web.rs:1 `use crate::auth::get_user;` 1.0",
            "src/web.rs:4 `get_user(id) + get_user(2)` 1.0",
            "src/web.rs:4 `get_user(id) + get_user(2)` 1.0",
        ],
        &|_| {},
    )
    .await;
}

#[tokio::test]
async fn the_typescript_server_also_reads_javascript() {
    let Some(typescript) = typescript_package().filter(|_| installed("typescript-language-server")) else {
        skip("typescript-language-server, or a TypeScript with a tsserver for it,");
        return;
    };
    let web =
        "import { getUser } from \"./auth\";\n\nexport function profile(id) {\n  return getUser(id);\n}\n";
    follows_into_a_lane(
        "typescript-language-server",
        &["typescript-language-server", "--stdio"],
        Lang::JavaScript,
        &[
            ("jsconfig.json", "{ \"compilerOptions\": { \"module\": \"esnext\", \"moduleResolution\": \"bundler\" }, \"include\": [\"src\"] }\n"),
            ("src/auth.js", "export function getUser(id) {\n  return id;\n}\n"),
            ("src/web.js", web),
        ],
        ("src/auth.js", "getUser"),
        &[("src/web.js", Some(&web.replace("return getUser(id);", "return getUser(id) + getUser(2);")))],
        &["src/web.js:1 `import { getUser } from \"./auth\";` 1.0", "src/web.js:4 `return getUser(id);` 1.0"],
        &[
            "src/web.js:1 `import { getUser } from \"./auth\";` 1.0",
            "src/web.js:4 `return getUser(id) + getUser(2);` 1.0",
            "src/web.js:4 `return getUser(id) + getUser(2);` 1.0",
        ],
        &|root| {
            std::fs::create_dir_all(root.join("node_modules")).unwrap();
            std::os::unix::fs::symlink(&typescript, root.join("node_modules/typescript")).unwrap();
        },
    )
    .await;
}

#[tokio::test]
async fn the_c_server_also_reads_cpp() {
    if !installed("clangd") {
        skip("clangd");
        return;
    }
    let web = "#include \"auth.hpp\"\n\nint profile(int id) {\n  return app::get_user(id);\n}\n";
    follows_into_a_lane(
        "clangd",
        &["clangd"],
        Lang::Cpp,
        &[
            ("auth.hpp", "namespace app {\nint get_user(int id);\n}\n"),
            (
                "auth.cpp",
                "#include \"auth.hpp\"\n\nnamespace app {\nint get_user(int id) {\n  return id;\n}\n\nint current() {\n  return get_user(0);\n}\n}\n",
            ),
            ("web.cpp", web),
        ],
        ("auth.cpp", "app.get_user"),
        &[(
            "web.cpp",
            Some(&web.replace("return app::get_user(id);", "return app::get_user(id) + app::get_user(2);")),
        )],
        &["auth.cpp:9 `return get_user(0);` 1.0", "web.cpp:4 `return app::get_user(id);` 1.0"],
        &[
            "auth.cpp:9 `return get_user(0);` 1.0",
            "web.cpp:4 `return app::get_user(id) + app::get_user(2);` 1.0",
            "web.cpp:4 `return app::get_user(id) + app::get_user(2);` 1.0",
        ],
        &|root| {
            let entry = |file: &str| {
                format!(
                    "{{ \"directory\": {:?}, \"file\": {file:?}, \"command\": \"c++ -I. -c {file}\" }}",
                    root.display().to_string()
                )
            };
            let commands = format!("[{}, {}]\n", entry("auth.cpp"), entry("web.cpp"));
            std::fs::write(root.join("compile_commands.json"), commands).unwrap();
        },
    )
    .await;
}
