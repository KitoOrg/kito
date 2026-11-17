//! A sandbox for what runs on behalf of a lane.
//!
//! The checks of an integration run code a lane wrote, as the user that
//! runs the daemon: on a server, next to other people's repositories and
//! the server's own keys. So does anything an agent starts in its lane.
//! Confined, such a command:
//!
//! - writes only where it is told it may: its own copy of the files and a
//!   temporary directory;
//! - does not read what is hidden from it: Kito's own data, the other
//!   repositories of a server, the keys of the user that runs it;
//! - opens no TCP connection, when it is given no network.
//!
//! It is done with Landlock (Linux 5.13; the network part needs 6.7): a
//! process gives up rights for itself and everything it starts, with no
//! privilege and no way back. The daemon does not confine itself: it
//! starts `kitod sandbox <policy> -- <command>`, which gives up the rights
//! and becomes the command.
//!
//! Landlock says what is allowed, not what is forbidden. "Everything but
//! these directories" is spelled as every sibling of the hidden ones, all
//! the way up to the root: [`open_paths`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// What a confined command may do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Where it may write (and read): directories, or single files.
    pub write: Vec<PathBuf>,
    /// What it may not read. Everything else, it may.
    #[serde(default)]
    pub hide: Vec<PathBuf>,
    /// What it may read all the same, inside what is hidden.
    #[serde(default)]
    pub show: Vec<PathBuf>,
    /// It may open TCP connections and listen for them.
    #[serde(default)]
    pub network: bool,
    /// What its environment has more than the daemon's.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Devices every program expects to be able to write to.
const DEVICES: [&str; 6] = ["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty"];

/// The path with its links followed, if it exists.
fn real(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}

/// Everything under `root` except `hidden` (real paths), as the fewest
/// paths that say it: a directory with nothing hidden in it is named
/// whole; one that holds something hidden is opened up, entry by entry.
/// Links are left out: what they lead to is named where it really is.
pub fn open_paths(root: &Path, hidden: &[PathBuf]) -> Vec<PathBuf> {
    let mut open = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else { return open };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() || hidden.contains(&path) {
            continue;
        }
        match kind.is_dir() && hidden.iter().any(|hidden| hidden.starts_with(&path)) {
            true => open.extend(open_paths(&path, hidden)),
            false => open.push(path),
        }
    }
    open
}

impl Policy {
    /// The same policy with every path as it really is (links followed),
    /// and without the ones that do not exist.
    pub fn settled(&self) -> Policy {
        let reals =
            |paths: &[PathBuf]| -> Vec<PathBuf> { paths.iter().filter_map(|path| real(path)).collect() };
        Policy {
            write: reals(&self.write),
            hide: reals(&self.hide),
            show: reals(&self.show),
            network: self.network,
            env: self.env.clone(),
        }
    }
}

/// A path of a `[sandbox]` list: `~` is the home of the user that runs the
/// command, any other path that is not absolute starts at `root`.
fn configured(root: &Path, text: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    match (text, text.strip_prefix("~/")) {
        ("~", _) => home(),
        (_, Some(rest)) => home().join(rest),
        _ => root.join(text),
    }
}

/// What a repository kept by a server (`<server>/repos/<name>`) has around
/// it that its commands do not read: the other repositories, and the
/// server's own files (who may connect, its certificate).
fn around(root: &Path) -> Vec<PathBuf> {
    let Some(repos) = root.parent().filter(|dir| dir.ends_with("repos")) else { return Vec::new() };
    let Some(server) = repos.parent().filter(|dir| dir.join("server.toml").is_file()) else {
        return Vec::new();
    };
    let others = |dir: &Path, but: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path != but)
            .collect()
    };
    let mut hidden = others(server, repos);
    hidden.extend(others(repos, root));
    hidden
}

/// What is common to every command confined for a repository: Kito's own
/// data is hidden, and what the repository's configuration adds.
fn base(root: &Path, config: &crate::config::SandboxConfig, private: &[PathBuf], network: bool) -> Policy {
    for dir in private {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut hide = vec![root.join(kito_store::STORE_DIR)];
    hide.extend(around(root));
    hide.extend(config.hide.iter().map(|path| configured(root, path)));
    let mut write = private.to_vec();
    write.extend(config.write.iter().map(|path| configured(root, path)));
    Policy { write, hide, show: private.to_vec(), network, env: BTreeMap::new() }
}

/// The policy for the checks of an integration: they write in the copy of
/// the merged result, in a temporary directory emptied before each run and
/// in a cache of their own, all under `.kito/verify`.
pub fn for_checks(root: &Path, config: &crate::config::SandboxConfig) -> Policy {
    let verify = root.join(kito_store::STORE_DIR).join("verify");
    let (work, tmp, cache) = (verify.join("work"), verify.join("tmp"), verify.join("cache"));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut policy = base(root, config, &[work, tmp.clone(), cache.clone()], config.network);
    policy.env = private_env(&tmp, Some(&cache));
    policy
}

/// The policy for what an agent starts in its lane: it writes in the
/// lane's working copy and in a temporary directory next to it.
pub fn for_lane(root: &Path, workdir: &Path, config: &crate::config::SandboxConfig, network: bool) -> Policy {
    let tmp = workdir.parent().unwrap_or(workdir).join("tmp");
    let mut policy = base(root, config, &[workdir.to_owned(), tmp.clone()], network);
    policy.env = private_env(&tmp, None);
    policy
}

/// A command that will run in the working copy of a lane, confined to it:
/// add the program and its arguments. An error if this system cannot
/// confine a command.
pub fn lane_command(root: &Path, workdir: &Path, network: bool) -> Result<std::process::Command, String> {
    let helper = helper().filter(|_| available()).ok_or_else(|| {
        "a command cannot be confined here: it needs Linux 5.13 or later, and `kitod` next to `kito`"
            .to_owned()
    })?;
    // A directory connected to a server has no configuration of its own.
    let config = crate::config::Config::load(&root.join(kito_store::STORE_DIR))
        .map(|config| config.sandbox)
        .unwrap_or_default();
    let prefix = wrap(&for_lane(root, workdir, &config, network), &helper);
    let mut command = std::process::Command::new(&prefix[0]);
    command.args(&prefix[1..]).current_dir(workdir);
    Ok(command)
}

/// Runs `sh -c <script>` in the working copy of a lane, confined to it, for
/// at most `timeout`. Returns how it ended (`None`: stopped for taking too
/// long, or by a signal) and the end of what it printed.
pub fn run_in_lane(
    root: &Path,
    workdir: &Path,
    network: bool,
    script: &str,
    timeout: std::time::Duration,
) -> Result<(Option<i32>, String), String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    const KEPT: usize = 8000;
    let mut child = lane_command(root, workdir, network)?
        .arg("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A group of its own, so that everything it started stops with it.
        .process_group(0)
        .spawn()
        .map_err(|e| format!("the command could not be started: {e}"))?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let out = read(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let err = read(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let deadline = std::time::Instant::now() + timeout;
    let mut late = false;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break Some(status),
            None if std::time::Instant::now() >= deadline => {
                late = true;
                // The whole group: `kill` with a negative number.
                let _ = std::process::Command::new("kill")
                    .arg("-KILL")
                    .arg(format!("-{}", child.id()))
                    .stderr(Stdio::null())
                    .status();
                let _ = child.kill();
                break child.wait().ok();
            }
            None => std::thread::sleep(std::time::Duration::from_millis(15)),
        }
    };
    let mut bytes = out.join().unwrap_or_default();
    bytes.extend(err.join().unwrap_or_default());
    if bytes.len() > KEPT {
        bytes.drain(..bytes.len() - KEPT);
    }
    let mut output = String::from_utf8_lossy(&bytes).trim_end().to_owned();
    if late {
        output.push_str(&format!("\n(still running after {} s: stopped)", timeout.as_secs()));
    }
    Ok((status.and_then(|status| status.code()).filter(|_| !late), output))
}

/// The environment that sends temporary files, and the caches of the
/// usual tools, where a confined command may write.
fn private_env(tmp: &Path, cache: Option<&Path>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let text = |path: &Path| path.display().to_string();
    for name in ["TMPDIR", "TMP", "TEMP"] {
        env.insert(name.to_owned(), text(tmp));
    }
    if let Some(cache) = cache {
        env.insert("XDG_CACHE_HOME".to_owned(), text(cache));
        env.insert("GOCACHE".to_owned(), text(&cache.join("go-build")));
        env.insert("npm_config_cache".to_owned(), text(&cache.join("npm")));
        env.insert("PIP_CACHE_DIR".to_owned(), text(&cache.join("pip")));
    }
    env.insert("KITO_SANDBOX".to_owned(), "1".to_owned());
    env
}

/// The program that confines: the `kitod` named by `KITOD_BIN`, this
/// program if it is `kitod`, or the `kitod` next to it.
pub fn helper() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("KITOD_BIN").map(PathBuf::from).filter(|path| path.is_file()) {
        return Some(named);
    }
    let this = std::env::current_exe().ok()?;
    let name = format!("kitod{}", std::env::consts::EXE_SUFFIX);
    if this.file_name().is_some_and(|file| file == name.as_str()) {
        return Some(this);
    }
    Some(this.with_file_name(name)).filter(|path| path.is_file())
}

/// What to put before a command to run it confined:
/// `kitod sandbox <policy> --`.
pub fn wrap(policy: &Policy, helper: &Path) -> Vec<OsString> {
    let policy = serde_json::to_string(&policy.settled()).unwrap_or_default();
    vec![helper.into(), "sandbox".into(), policy.into(), "--".into()]
}

/// `true` if this system can confine a process.
pub fn available() -> bool {
    imp::available()
}

/// `kitod sandbox <policy> -- <program> <arguments>`: gives up the rights
/// the policy does not grant, then becomes the program. Leaves with 126 if
/// the process could not be confined, 127 if the program could not start.
pub fn run(mut args: impl Iterator<Item = String>) -> ExitCode {
    let policy = args.next().and_then(|json| serde_json::from_str::<Policy>(&json).ok());
    let (Some(policy), Some("--")) = (policy, args.next().as_deref()) else {
        eprintln!("usage: kitod sandbox <policy as JSON> -- <program> [arguments]");
        return ExitCode::from(2);
    };
    let Some(program) = args.next() else {
        eprintln!("kitod sandbox: no program to run");
        return ExitCode::from(2);
    };
    if let Err(why) = imp::confine(&policy) {
        eprintln!("kitod sandbox: this command was not run, because it could not be confined: {why}");
        return ExitCode::from(126);
    }
    let failed = imp::become_program(&program, args, &policy.env);
    eprintln!("kitod sandbox: {program}: {failed}");
    ExitCode::from(127)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{open_paths, Policy, DEVICES};
    use landlock::{
        path_beneath_rules, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, ABI,
    };
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// The file rights that are given up. Not the newer ones on devices:
    /// a terminal is asked things by every shell.
    const FILES: ABI = ABI::V4;

    pub fn available() -> bool {
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V1))
            .and_then(|ruleset| ruleset.create())
            .is_ok()
    }

    pub fn confine(policy: &Policy) -> Result<(), String> {
        let fail = |e: landlock::RulesetError| e.to_string();
        let mut ruleset = Ruleset::default().handle_access(AccessFs::from_all(FILES)).map_err(fail)?;
        if !policy.network {
            // No rule allows a port: no TCP connection is made or accepted.
            ruleset = ruleset.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp).map_err(fail)?;
        }
        // Readable: everything but what is hidden, and what is shown inside it.
        let mut readable: Vec<PathBuf> = match policy.hide.is_empty() {
            true => vec![PathBuf::from("/")],
            false => open_paths(Path::new("/"), &policy.hide),
        };
        readable.extend(policy.show.iter().cloned());
        let writable = policy.write.iter().map(PathBuf::as_path).chain(DEVICES.iter().map(Path::new));
        let status = ruleset
            .create()
            .map_err(fail)?
            .add_rules(path_beneath_rules(&readable, AccessFs::from_read(FILES)))
            .map_err(fail)?
            .add_rules(path_beneath_rules(writable, AccessFs::from_all(FILES)))
            .map_err(fail)?
            .restrict_self()
            .map_err(fail)?;
        match status.ruleset {
            RulesetStatus::NotEnforced => Err("this system has no Landlock (Linux 5.13 or later)".to_owned()),
            _ => Ok(()),
        }
    }

    /// Becomes the program: returns only if it could not.
    pub fn become_program(
        program: &str,
        args: impl Iterator<Item = String>,
        env: &BTreeMap<String, String>,
    ) -> std::io::Error {
        use std::os::unix::process::CommandExt;
        std::process::Command::new(program).args(args).envs(env).exec()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::Policy;
    use std::collections::BTreeMap;

    pub fn available() -> bool {
        false
    }

    pub fn confine(_policy: &Policy) -> Result<(), String> {
        Err("confining a command needs Linux (Landlock)".to_owned())
    }

    pub fn become_program(
        program: &str,
        _args: impl Iterator<Item = String>,
        _env: &BTreeMap<String, String>,
    ) -> std::io::Error {
        std::io::Error::other(format!("{program} was not started"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_but_what_is_hidden_is_spelled_as_its_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in [
            "usr/bin",
            "srv/kito/repos/shop/.kito/verify",
            "srv/kito/repos/other",
            "srv/www",
            "home/ada/.ssh",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("srv/kito/server.toml"), "").unwrap();
        std::fs::write(root.join("home/ada/notes.txt"), "").unwrap();
        std::os::unix::fs::symlink(root.join("srv/kito"), root.join("data")).unwrap();

        let hidden = [root.join("srv/kito"), root.join("home/ada/.ssh")];
        let mut open: Vec<String> = open_paths(&root, &hidden)
            .iter()
            .map(|path| path.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        open.sort();
        // `usr` whole; `srv` and `home/ada` entry by entry; the link to what is hidden, not at all.
        assert_eq!(open, ["home/ada/notes.txt", "srv/www", "usr"]);

        // Nothing hidden: every entry of the root, whole.
        let mut all: Vec<String> = open_paths(&root, &[])
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        all.sort();
        assert_eq!(all, ["home", "srv", "usr"]);
    }

    #[test]
    fn a_policy_is_settled_on_real_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("work")).unwrap();
        std::os::unix::fs::symlink(root.join("work"), root.join("link")).unwrap();
        let policy = Policy {
            write: vec![root.join("link"), root.join("missing")],
            hide: vec![root.join("nothing")],
            ..Policy::default()
        };
        let settled = policy.settled();
        assert_eq!(settled.write, [root.join("work")]);
        assert!(settled.hide.is_empty());
    }
}
