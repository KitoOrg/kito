//! The checks of an integration run confined: they write only in their own
//! copy of the merged result, do not read Kito's own data or what the
//! repository hides, and open no TCP connection.

use kito_core::paths::Canonical as _;
use kito_integrate::Outcome;
use kitod::Repo;
use std::fs;
use std::path::{Path, PathBuf};

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let empty = std::env::temp_dir().join(format!("kito-test-config-{}", std::process::id()));
        std::env::set_var("KITO_CONFIG_DIR", &empty);
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("KITO_AUTHOR", "ada");
        // The program that confines: the daemon's own binary.
        std::env::set_var("KITOD_BIN", env!("CARGO_BIN_EXE_kitod"));
    });
}

/// What a check written in a lane tries, each attempt on a line of
/// `result.txt` in its own directory. It always succeeds: the test reads
/// what happened.
fn probe(root: &Path, secret: &Path, port: u16) -> String {
    format!(
        r#"try() {{ if "$@" > /dev/null 2>&1; then echo "$LABEL: yes"; else echo "$LABEL: no"; fi; }}
{{
LABEL="writes in its own copy" try sh -c 'echo x > made-here.txt'
LABEL="writes in its temporary directory" try sh -c 'echo x > "$TMPDIR/scratch"'
LABEL="writes in the main directory" try sh -c 'echo x > "{root}/hacked.txt"'
LABEL="writes in the home" try sh -c 'echo x > "$HOME/.kito-test-hacked"'
LABEL="reads the code" try cat "{root}/src/shop.py"
LABEL="reads the configuration of Kito" try cat "{root}/.kito/config.toml"
LABEL="reads the key of the socket" try cat "{root}/.kito/socket.key"
LABEL="lists the lanes" try ls "{root}/.kito/lanes"
LABEL="reads what is hidden" try cat "{secret}"
LABEL="opens a connection" try python3 -c 'import socket; socket.create_connection(("127.0.0.1", {port}), 2)'
LABEL="is told it is confined" try test "$KITO_SANDBOX" = 1
}} > result.txt
"#,
        root = root.display(),
        secret = secret.display(),
    )
}

fn results(root: &Path) -> Vec<(String, bool)> {
    let text = fs::read_to_string(root.join(".kito/verify/work/result.txt")).expect("the check ran");
    text.lines()
        .filter_map(|line| line.rsplit_once(": "))
        .map(|(what, answer)| (what.to_owned(), answer == "yes"))
        .collect()
}

/// A repository whose check is a script of the lane, with a file outside
/// it that the repository hides.
fn shop(mode: &str, network: bool) -> (tempfile::TempDir, PathBuf, Repo, std::net::TcpListener) {
    setup();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonical().unwrap();
    let root = base.join("shop");
    fs::create_dir(&root).unwrap();
    write(&base, "keys/deploy.key", "a secret\n");
    drop(Repo::init(&root).unwrap());
    let config = root.join(".kito/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    let text = text
        .replace("# commands = [\"npx tsc --noEmit\", \"npm test\"]", "commands = [\"sh check.sh\"]")
        .replace("mode = \"auto\"", &format!("mode = \"{mode}\""))
        .replace("network = false", &format!("network = {network}"))
        .replace("hide = [\"~/.ssh\"", &format!("hide = [\"{}\", \"~/.ssh\"", base.join("keys").display()));
    assert!(text.contains("sh check.sh") && text.contains(&format!("mode = \"{mode}\"")), "{text}");
    fs::write(&config, text).unwrap();
    let mut repo = Repo::open(&root).unwrap();
    write(&root, ".gitignore", ".kito/\n");
    write(&root, "src/shop.py", "def total(prices):\n    return sum(prices)\n");
    write(&root, "check.sh", "true\n");
    repo.checkpoint("start").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    (tmp, root, repo, listener)
}

fn integrate_probe(root: &Path, repo: &mut Repo, port: u16) -> kito_integrate::IntegrationResult {
    let scope = ["check.sh".to_owned()];
    let lane = repo.open_lane("mallory", None, None, "a check of my own", &scope).unwrap();
    let work = lane.workdir.clone().unwrap();
    let secret = root.parent().unwrap().join("keys/deploy.key");
    write(&work, "check.sh", &probe(root, &secret, port));
    repo.integrate(lane.id.parse().unwrap()).unwrap()
}

#[test]
fn the_checks_of_a_lane_are_confined_to_their_copy() {
    if !kitod::sandbox::available() {
        eprintln!("this system cannot confine a process: nothing to test");
        return;
    }
    let (_tmp, root, mut repo, listener) = shop("auto", false);
    assert_eq!(repo.sandbox_state(), "confined");
    let result = integrate_probe(&root, &mut repo, listener.local_addr().unwrap().port());
    assert_eq!(result.result, Outcome::Integrated, "{result:?}");
    assert!(result.verification.as_ref().unwrap().confined);

    let found = results(&root);
    let expected = [
        ("writes in its own copy", true),
        ("writes in its temporary directory", true),
        ("writes in the main directory", false),
        ("writes in the home", false),
        ("reads the code", true),
        ("reads the configuration of Kito", false),
        ("reads the key of the socket", false),
        ("lists the lanes", false),
        ("reads what is hidden", false),
        ("opens a connection", false),
        ("is told it is confined", true),
    ];
    for (what, may) in expected {
        let did =
            found.iter().find(|(label, _)| label == what).unwrap_or_else(|| panic!("{what}: {found:?}"));
        assert_eq!(did.1, may, "{what}: {found:?}");
    }
    assert!(!root.join("hacked.txt").exists());
    // What the check wrote in its copy is not part of what was integrated.
    assert!(!root.join("made-here.txt").exists());
}

#[test]
fn a_repository_may_give_its_checks_the_network_or_no_sandbox() {
    if !kitod::sandbox::available() {
        return;
    }
    // With the network: a connection is made; the rest holds.
    let (_tmp, root, mut repo, listener) = shop("auto", true);
    assert_eq!(repo.sandbox_state(), "confined, with network");
    let result = integrate_probe(&root, &mut repo, listener.local_addr().unwrap().port());
    assert_eq!(result.result, Outcome::Integrated, "{result:?}");
    let found = results(&root);
    let did = |what: &str| found.iter().find(|(label, _)| label == what).unwrap().1;
    assert!(did("opens a connection"), "{found:?}");
    assert!(!did("writes in the main directory") && !did("reads what is hidden"), "{found:?}");

    // Turned off: the checks run as the daemon does, and the result says so.
    let (_tmp, root, mut repo, listener) = shop("off", false);
    assert_eq!(repo.sandbox_state(), "off");
    let result = integrate_probe(&root, &mut repo, listener.local_addr().unwrap().port());
    assert_eq!(result.result, Outcome::Integrated, "{result:?}");
    assert!(!result.verification.as_ref().unwrap().confined);
    let found = results(&root);
    let did = |what: &str| found.iter().find(|(label, _)| label == what).unwrap().1;
    assert!(did("writes in the main directory") && did("reads the configuration of Kito"), "{found:?}");
}

#[test]
fn a_check_that_fails_confined_says_that_it_was() {
    if !kitod::sandbox::available() {
        return;
    }
    let (_tmp, root, mut repo, _listener) = shop("required", false);
    let lane = repo.open_lane("bob", None, None, "a note", &["check.sh".to_owned()]).unwrap();
    let work = lane.workdir.clone().unwrap();
    write(&work, "check.sh", &format!("echo run > {}/out.txt\n", root.display()));
    let result = repo.integrate(lane.id.parse().unwrap()).unwrap();
    assert_eq!(result.result, Outcome::VerificationFailed, "{result:?}");
    let output = result.verification.unwrap().output.unwrap();
    assert!(output.contains("Permission denied"), "{output}");
    assert!(output.contains("the checks run confined"), "{output}");
    assert!(output.contains("[sandbox]"), "{output}");
}
