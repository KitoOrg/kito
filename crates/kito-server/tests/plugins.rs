//! Plugins enabled by a server (ADR 0006): their commands, and the
//! server's events. The plugin is `kito-hello`, an example of `kito-cli`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The daemon binary sits next to the server's. If the tests run for this
/// package alone it may not have been built.
fn kitod() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    let path = Path::new(env!("CARGO_BIN_EXE_kito-server"))
        .with_file_name(format!("kitod{}", std::env::consts::EXE_SUFFIX));
    BUILD.call_once(|| {
        if !path.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "kitod", "--bin", "kitod"])
                .status()
                .unwrap();
            assert!(status.success(), "building kitod");
        }
    });
    path
}

fn server(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kito-server"))
        .arg("--dir")
        .arg(dir)
        .args(args)
        .env("KITOD_BIN", kitod())
        .env("KITO_AUTHOR", "the server")
        .env_remove("KITO_EMAIL")
        // Where the user the server runs as has their plugins, for the tests.
        .env("KITO_CONFIG_DIR", dir.join(".personal-config"))
        .output()
        .unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = server(dir, args);
    let (said, errors) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "kito-server {args:?}: {said}{errors}");
    said.into_owned()
}

/// What a command that must fail said.
fn refused(dir: &Path, args: &[&str]) -> String {
    let out = server(dir, args);
    assert!(!out.status.success(), "kito-server {args:?} was not refused");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A directory with the plugin of the tests in it, as a download would
/// leave it: its manifest and its program.
fn hello_plugin(base: &Path, manifest: &str) -> PathBuf {
    let name = format!("kito-hello{}", std::env::consts::EXE_SUFFIX);
    let built = Path::new(env!("CARGO_BIN_EXE_kito-server")).with_file_name("examples").join(&name);
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        if !built.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "kito-cli", "--example", "kito-hello"])
                .status()
                .unwrap();
            assert!(status.success(), "building kito-hello");
        }
    });
    let dir = base.join("hello-src");
    fs::create_dir_all(&dir).unwrap();
    fs::copy(built, dir.join(&name)).unwrap();
    fs::write(dir.join("kito-plugin.toml"), manifest).unwrap();
    dir
}

const HELLO: &str = r#"name = "hello"
version = "0.4.0"
api = 1
permissions = ["server_events", "server_admin"]

[[server_event]]
kinds = ["user_added", "user_removed", "role_changed", "repo_added", "repo_changed"]

[[server_command]]
name = "greet"
about = "Says where it runs"
"#;

fn new_server() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap().join("server");
    ok(tmp.path(), &["init", dir.to_str().unwrap()]);
    (tmp, dir)
}

#[test]
fn a_plugin_runs_on_a_server_that_enabled_it_and_adds_its_commands() {
    let (tmp, dir) = new_server();
    assert!(ok(&dir, &["plugin", "list"]).contains("No plugins."));
    assert!(refused(&dir, &["greet"]).contains("`greet` is not a command of kito-server"));
    assert!(refused(&dir, &["plugin", "enable", "hello"]).contains("is not installed"));

    // Installed for the user the server runs as: it runs nowhere yet.
    let source = hello_plugin(tmp.path(), HELLO);
    let added = ok(&dir, &["plugin", "add", "--path", source.to_str().unwrap()]);
    assert!(added.starts_with("hello 0.4.0 is installed in "), "{added}");
    let not_here = refused(&dir, &["greet"]);
    assert!(
        not_here.contains("which this server did not enable: `kito-server plugin enable hello`"),
        "{not_here}"
    );

    // Enabled, with what it asks for: its command is the server's.
    let enabled = ok(&dir, &["plugin", "enable", "hello"]);
    assert!(
        enabled.starts_with("hello is enabled on this server. It may: server_events, server_admin.\n"),
        "{enabled}"
    );
    assert!(
        enabled.contains("  kito-server greet\n") && enabled.contains("  run for user_added,"),
        "{enabled}"
    );
    let config = fs::read_to_string(dir.join("server.toml")).unwrap();
    assert!(
        config
            .contains("[plugins.hello]\nenabled = true\npermissions = [\"server_events\", \"server_admin\"]"),
        "{config}"
    );
    assert_eq!(ok(&dir, &["greet", "to", "all"]), "hello from a server: to all\n");
    assert_eq!(server(&dir, &["greet", "fail"]).status.code(), Some(5), "its exit code is the server's");
    let listed = ok(&dir, &["plugin", "list"]);
    assert!(listed.starts_with("hello 0.4.0  [enabled (server_events, server_admin)]\n"), "{listed}");

    // Given less than it asks for, and never more.
    ok(&dir, &["plugin", "enable", "hello", "--only", "server_events"]);
    assert!(refused(&dir, &["greet"]).contains("it needs the permission `server_admin`"));
    assert!(refused(&dir, &["plugin", "enable", "hello", "--only", "sign_in"])
        .contains("does not ask for `sign_in`"));
    assert!(
        refused(&dir, &["plugin", "enable", "hello", "--only", "everything"]).contains("is not a permission")
    );

    // Disabled, nothing of it runs here.
    assert_eq!(ok(&dir, &["plugin", "disable", "hello"]), "hello is disabled on this server.\n");
    assert!(refused(&dir, &["greet"]).contains("did not enable"));
    assert!(ok(&dir, &["plugin", "list"]).contains("[disabled]"));
}

#[test]
fn a_plugin_is_told_what_happens_on_the_server() {
    let (tmp, dir) = new_server();
    let source = hello_plugin(tmp.path(), HELLO);
    ok(&dir, &["plugin", "add", "--path", source.to_str().unwrap()]);
    let log = dir.join("plugins/hello/events.log");
    let told = || fs::read_to_string(&log).unwrap_or_default();

    // Not enabled: told nothing.
    ok(&dir, &["user", "add", "ada"]);
    assert_eq!(told(), "");
    ok(&dir, &["plugin", "enable", "hello"]);

    // After the fact, in order, and only the kinds it listed.
    ok(&dir, &["repo", "add", "shop"]);
    ok(&dir, &["user", "add", "bob", "--repo", "shop:write"]);
    ok(&dir, &["grant", "bob", "shop", "read"]);
    ok(&dir, &["repo", "set", "shop", "--approvals", "1"]);
    ok(&dir, &["user", "remove", "bob"]);
    assert_eq!(
        told().lines().collect::<Vec<_>>(),
        [
            "repo_added shop",
            "user_added bob (member)",
            "role_changed bob may read in shop",
            "repo_changed shop  [main open to direct edits, no checks, 1 approval]",
            "user_removed bob",
        ]
    );

    // One that fails is said where the administrator is, and kept until
    // it runs well again; what was asked is done all the same.
    let out = server(&dir, &["user", "add", "explode"]);
    assert!(out.status.success(), "adding the person is not undone by a plugin");
    let warned = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        warned.contains("the plugin `hello` failed on `user_added`: exit code 1: hello: it exploded"),
        "{warned}"
    );
    assert!(ok(&dir, &["user", "list"]).contains("explode"));
    assert!(ok(&dir, &["plugin", "list"]).contains("it last failed on `user_added`"));
    // The next event it handles well clears it.
    ok(&dir, &["user", "add", "dave"]);
    assert!(!ok(&dir, &["plugin", "list"]).contains("last failed"));

    // Without the permission it is told nothing more.
    ok(&dir, &["plugin", "enable", "hello", "--only", "server_admin"]);
    ok(&dir, &["user", "add", "carol"]);
    assert!(!told().contains("carol"), "{}", told());
    let daemon = kitod::client::Client::connect(&dir.join("repos/shop"));
    if let Ok(mut daemon) = daemon {
        let _ = daemon.call_value("shutdown", serde_json::json!({}));
    }
}

/// A command of the server, run until it ends or for `within` at most:
/// whether it ended by then, and how it ended. `closed`: its standard
/// error is closed before it writes anything there, as when the SSH
/// connection of `kito-server stdio` has gone.
fn within(dir: &Path, args: &[&str], within: std::time::Duration, closed: bool) -> Option<bool> {
    use std::process::Stdio;
    let mut command = Command::new(env!("CARGO_BIN_EXE_kito-server"));
    command
        .arg("--dir")
        .arg(dir)
        .args(args)
        .env("KITOD_BIN", kitod())
        .env("KITO_CONFIG_DIR", dir.join(".personal-config"))
        .stdout(Stdio::null())
        .stderr(if closed { Stdio::piped() } else { Stdio::null() });
    let mut child = command.spawn().unwrap();
    drop(child.stderr.take());
    let asked = std::time::Instant::now();
    while asked.elapsed() < within {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status.success());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[test]
fn a_plugin_that_leaves_a_child_or_a_stderr_that_is_gone_stops_nothing() {
    let (tmp, dir) = new_server();
    let source = hello_plugin(tmp.path(), HELLO);
    ok(&dir, &["plugin", "add", "--path", source.to_str().unwrap()]);
    ok(&dir, &["plugin", "enable", "hello"]);
    let log = dir.join("plugins/hello/events.log");
    let told = || fs::read_to_string(&log).unwrap_or_default();
    let second = std::time::Duration::from_secs(1);

    // It leaves a child behind that keeps its output open for 20 seconds:
    // it was told, and the command goes on long before that.
    assert_eq!(within(&dir, &["user", "add", "linger"], second * 10, false), Some(true));
    assert!(told().contains("user_added linger (member)"), "{}", told());
    // And the next is told as well.
    ok(&dir, &["user", "add", "dave"]);
    assert!(told().contains("user_added dave (member)"), "{}", told());

    // Nobody reads what the server says any more: a plugin that fails is
    // said nowhere, the next event is told, and the command ends.
    assert_eq!(within(&dir, &["user", "add", "explode"], second * 10, true), Some(true));
    assert_eq!(within(&dir, &["user", "add", "erin"], second * 10, true), Some(true));
    assert!(told().contains("user_added erin (member)"), "{}", told());
    let listed = ok(&dir, &["user", "list"]);
    assert!(listed.contains("explode") && listed.contains("erin"), "{listed}");
}

const LEAVES: &str = r#"name = "hello"
version = "0.9.0"
api = 2
permissions = ["server_events"]

[[server_event]]
kinds = ["user_added"]

[cleanup]
about = "Takes out what it set up"
"#;

#[test]
fn a_plugin_is_taken_off_a_server_as_cleanly_as_it_went_on() {
    let (tmp, dir) = new_server();
    let source = hello_plugin(tmp.path(), LEAVES);
    let told = tmp.path().join("cleaned");
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_kito-server"))
            .arg("--dir")
            .arg(&dir)
            .args(args)
            .env("KITOD_BIN", kitod())
            .env("KITO_AUTHOR", "the server")
            .env("KITO_CONFIG_DIR", dir.join(".personal-config"))
            .env("KITO_HELLO_CLEANED", &told)
            .output()
            .unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    };
    assert!(run(&["plugin", "add", "--path", source.to_str().unwrap()]).0);
    assert!(run(&["plugin", "enable", "hello"]).0);
    // It keeps something of its own on the server.
    assert!(run(&["user", "add", "ada"]).0);
    let own = dir.join("plugins/hello");
    assert!(own.join("events.log").is_file());

    // What a server runs is not pulled from under it.
    let (ok, said) = run(&["plugin", "remove", "hello"]);
    assert!(
        !ok && said.contains("is enabled on this server: `kito-server plugin disable hello` first"),
        "{said}"
    );

    // Disabled: run one last time, as the server's. What it kept stays.
    let (ok, said) = run(&["plugin", "disable", "hello"]);
    assert!(ok && said.contains("hello is disabled on this server."), "{said}");
    assert_eq!(fs::read_to_string(&told).unwrap(), "server server=true\n");
    assert!(own.join("events.log").is_file());
    // With --purge, that goes too; with --no-cleanup, it is not run.
    assert!(run(&["plugin", "enable", "hello"]).0);
    fs::remove_file(&told).unwrap();
    let (ok, said) = run(&["plugin", "disable", "hello", "--purge", "--no-cleanup"]);
    assert!(ok && said.contains("and what it kept here is removed"), "{said}");
    assert!(!told.exists() && !own.exists());

    // Removed from the machine: one last time for the server's user, and
    // the server says nothing of it any more.
    let (ok, said) = run(&["plugin", "remove", "hello"]);
    assert!(ok && said.contains("hello is removed from this machine."), "{said}");
    assert_eq!(fs::read_to_string(&told).unwrap(), "person server=false\n");
    assert!(!dir.join(".personal-config/plugins/hello").exists());
    assert!(!run(&["plugin", "list"]).1.contains("hello"));
    assert!(!fs::read_to_string(dir.join("server.toml")).unwrap().contains("hello"));
    let (ok, said) = run(&["plugin", "remove", "hello"]);
    assert!(!ok && said.contains("the plugin `hello` is not installed"), "{said}");
}
