//! Plugins (ADR 0005): programs named `kito-<name>`, installed for a
//! person, enabled by a repository, and answered by the daemon within the
//! permissions the repository gave them.
//!
//! Nothing of a plugin is loaded here. This module reads manifests, keeps
//! the folder plugins are installed in, makes the key each plugin says to
//! the daemon, and says which permission a method needs.

use crate::config::CONFIG_FILE;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The file that says what a plugin is, next to its executable.
pub const MANIFEST: &str = "kito-plugin.toml";
/// In a plugin's environment: the repository it runs for, the version of
/// the daemon's API, and the key it says to the daemon.
pub const ENV_REPO: &str = "KITO_REPO";
pub const ENV_API: &str = "KITO_API";
pub const ENV_KEY: &str = "KITO_PLUGIN_KEY";
/// Where the daemon listens: the path of its Unix socket, or on Windows
/// the name of its pipe. A plugin that is not written with Kito's own
/// client cannot work it out by itself.
pub const ENV_SOCKET: &str = "KITO_SOCKET";

/// What [`ENV_SOCKET`] says for a repository.
pub fn socket_name(root: &Path) -> String {
    #[cfg(windows)]
    {
        crate::ipc::pipe_name(root)
    }
    #[cfg(not(windows))]
    {
        crate::rpc::socket_path(root).display().to_string()
    }
}

/// A group of methods a plugin may be given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// See: status, lanes, the board, the log, why, diffs, comments,
    /// reviews, notes, events, contents.
    Read,
    /// Work in lanes: open and close them, save files, set intents,
    /// extend scopes, comment, give verdicts, leave notes.
    Write,
    /// Ask for integrations, resolve conflicts, catch up.
    Integrate,
    /// Protect, configure, checkpoint, roll back, plugins.
    Admin,
    /// On a server (ADR 0006): say who somebody is. Not what they may do.
    SignIn,
    /// On a server: be reached through its HTTP address.
    Pages,
    /// On a server: be told what happens on it.
    ServerEvents,
    /// On a server: add commands to `kito-server`.
    ServerAdmin,
}

impl Permission {
    /// `true` for what a repository gives; the rest is a server's to give.
    pub fn of_a_repository(self) -> bool {
        matches!(self, Permission::Read | Permission::Write | Permission::Integrate | Permission::Admin)
    }

    pub fn word(self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Write => "write",
            Permission::Integrate => "integrate",
            Permission::Admin => "admin",
            Permission::SignIn => "sign_in",
            Permission::Pages => "pages",
            Permission::ServerEvents => "server_events",
            Permission::ServerAdmin => "server_admin",
        }
    }
}

const READ: &[&str] = &[
    "status",
    "log",
    "why",
    "why_graph",
    "reasons",
    "notes",
    "comments",
    "review",
    "lanes",
    "lane_status",
    "lane_diff",
    "lane_files",
    "get_blobs",
    "board",
    "who_touches",
    "events",
    "conflicts",
    "integration_result",
    "plugins",
    // Asks its own counterpart on the server (ADR 0007).
    "plugin_call",
    "mirror_status",
    // Asks that what is already saved be recorded: no change of theirs.
    "sync",
    // Asked before an edit and after a command: the answer changes
    // nothing of the repository.
    "before_edit",
    "after_shell",
    "reasons_behind",
];

const WRITE: &[&str] = &[
    "open_lane",
    "close_lane",
    "extend_scope",
    "set_intent",
    "rename_symbol",
    "put_file",
    "remove_file",
    "claim_writer",
    "notices",
    "add_note",
    "remove_note",
    "comment",
    "resolve_comment",
    "verdict",
    "attach",
    "detach",
    "take",
];

const INTEGRATE: &[&str] = &["request_integration", "resolve_conflict", "catch_up", "pull"];

/// Never for a plugin, whatever it was given: its own key would be worth
/// every other's.
const OWNER_ONLY: &[&str] = &["plugin_key", "shutdown"];

/// What a method needs. `None`: anybody who reached the daemon may ask
/// (`ping`). A method this list does not know needs `admin`.
pub fn permission_of(method: &str) -> Option<Permission> {
    match method {
        "ping" => None,
        method if READ.contains(&method) => Some(Permission::Read),
        method if WRITE.contains(&method) => Some(Permission::Write),
        method if INTEGRATE.contains(&method) => Some(Permission::Integrate),
        _ => Some(Permission::Admin),
    }
}

/// Who is on a connection to the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// Whoever can read the repository's own data: `kito`, `kito-mcp`.
    Owner,
    /// A plugin, with what the repository gave it.
    Plugin { name: String, permissions: Vec<Permission> },
}

impl Access {
    /// Why a method is not for whoever is on the connection. `None`: it is.
    pub fn refusal(&self, method: &str) -> Option<String> {
        let Access::Plugin { name, permissions } = self else { return None };
        if OWNER_ONLY.contains(&method) {
            return Some(format!("`{method}` is not for plugins (asked by the plugin `{name}`)"));
        }
        let needed = permission_of(method)?;
        match permissions.contains(&needed) {
            true => None,
            false => Some(format!(
                "the plugin `{name}` may not ask `{method}`: it needs the permission `{}`, which this \
                 repository did not give it (`kito plugin enable {name}` gives what it asks for)",
                needed.word()
            )),
        }
    }
}

/// What a repository says of a plugin: `[plugins.<name>]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginConfig {
    pub enabled: bool,
    /// What it was given.
    pub permissions: Vec<Permission>,
    /// On a server: it runs in the directories connected to it too, for
    /// whoever has it installed (ADR 0007).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub for_clients: bool,
}

/// A command a plugin adds: `kito <name>`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Command {
    pub name: String,
    pub about: String,
}

/// Events a plugin is run for: `kito-<name> event`, with the event as
/// JSON on its input, one at a time and in order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventSpec {
    /// `integrated`, `commented`, `reviewed`…: as `kito watch` shows them.
    pub kinds: Vec<String>,
    /// Seconds after which it is stopped (default 30).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// A gate of integration: `kito-<name> gate <gate>` is asked before a
/// lane is integrated, after the checks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateSpec {
    pub name: String,
    pub about: String,
    /// Seconds after which it is stopped and counts as failed (default 60).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// A tool a plugin offers to agents: `kito-mcp` lists it as
/// `kito.<plugin>.<name>` and runs `kito-<plugin> tool <name>` with the
/// arguments as JSON on its input.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSpec {
    pub name: String,
    /// What it does and when to use it, for the agent.
    pub description: String,
    /// The JSON Schema of its arguments (an object).
    pub input: serde_json::Value,
    /// Seconds after which it is stopped (default 120).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// The coding assistant a plugin sets up for Kito: `kito-<plugin>
/// assistant setup` writes that assistant's files in a repository, and
/// `assistant remove` takes them out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssistantSpec {
    /// `codex`, `cursor`: what `kito assistant add` is asked for.
    pub name: String,
}

/// A plugin that has something to say to an agent when a session starts
/// (ADR 0007): `kito-<plugin> session` prints it, and Kito gives it after
/// its own rules, saying whose words they are.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSpec {
    /// What it tells, for whoever reads the manifest.
    pub about: String,
}

/// How a plugin signs people in on a server (ADR 0006): it says who
/// somebody is, and the server says what they may do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SignInSpec {
    /// `github`: what `kito connect --sign-in` is asked for.
    pub method: String,
    pub about: String,
}

/// The pages a plugin serves through a server's HTTP address.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PagesSpec {
    /// The path its pages are under: `/`, `/metrics`.
    pub mount: String,
}

/// What a plugin is, as its `kito-plugin.toml` says.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: String,
    /// The version of the daemon's API it was written for.
    pub api: u32,
    /// What it asks to be given.
    pub permissions: Vec<Permission>,
    #[serde(rename = "command", skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Command>,
    #[serde(rename = "event", skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EventSpec>,
    #[serde(rename = "gate", skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<GateSpec>,
    #[serde(rename = "tool", skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant: Option<AssistantSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionSpec>,
    // What it does for a server (ADR 0006).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<SignInSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<PagesSpec>,
    #[serde(rename = "server_event", skip_serializing_if = "Vec::is_empty")]
    pub server_events: Vec<EventSpec>,
    #[serde(rename = "server_command", skip_serializing_if = "Vec::is_empty")]
    pub server_commands: Vec<Command>,
}

/// Runs a program of a plugin for a repository, with `input` on its
/// standard input and what it needs to reach the daemon in its
/// environment, and returns what it printed. `Err`: why it failed, with
/// the end of what it said.
pub fn run_plugin(
    program: &Path,
    args: &[&str],
    root: &Path,
    key: &str,
    input: &[u8],
    timeout: std::time::Duration,
) -> Result<Vec<u8>, String> {
    let envs = [
        (ENV_REPO, root.as_os_str().to_owned()),
        (ENV_KEY, key.into()),
        (ENV_SOCKET, socket_name(root).into()),
    ];
    run_program(program, args, root, &envs, input, timeout)
}

/// Runs a program of a plugin in `dir`, with `envs` and the version of
/// the API in its environment and `input` on its standard input, and
/// returns what it printed. `Err`: why it failed.
pub fn run_program(
    program: &Path,
    args: &[&str],
    dir: &Path,
    envs: &[(&str, std::ffi::OsString)],
    input: &[u8],
    timeout: std::time::Duration,
) -> Result<Vec<u8>, String> {
    use std::io::{Read, Write};
    use std::process::{Command as Process, Stdio};
    let mut child = Process::new(program)
        .args(args)
        .current_dir(dir)
        .env(ENV_API, crate::rpc::API.to_string())
        .envs(envs.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    // What it prints is read while it runs: a pipe that fills stops it.
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut said = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut said);
            }
            said
        })
    };
    let printed = reader(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let errors = reader(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    if let Some(mut pipe) = child.stdin.take() {
        // One that does not read what it is told is its own business.
        let _ = pipe.write_all(input);
    }
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("still running after {} s: stopped", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let (printed, errors) = (printed.join().unwrap_or_default(), errors.join().unwrap_or_default());
    if status.success() {
        return Ok(printed);
    }
    let how = status.code().map_or("killed by a signal".to_owned(), |code| format!("exit code {code}"));
    let said = String::from_utf8_lossy(&errors);
    let said: String = said.trim().chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
    Err(format!("{how}{}{said}", if said.is_empty() { "" } else { ": " }))
}

/// A gate, ready to be asked.
#[derive(Clone, Debug)]
pub struct GateRun {
    pub plugin: String,
    pub name: String,
    pub program: PathBuf,
    pub timeout: std::time::Duration,
}

/// A plugin that is run for events, and which.
#[derive(Clone, Debug)]
pub struct EventRun {
    pub plugin: String,
    pub program: PathBuf,
    pub kinds: Vec<String>,
    pub timeout: std::time::Duration,
    /// The key it says to the daemon.
    pub key: String,
    pub root: PathBuf,
}

/// A name a plugin, or a command of one, can have: what is safe as a
/// directory, in a configuration key and after `kito-`.
pub fn check_name(what: &str, name: &str) -> Result<(), String> {
    let plain = !name.is_empty()
        && name.len() <= 40
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && name.as_bytes()[0].is_ascii_lowercase();
    match plain {
        true => Ok(()),
        false => Err(format!(
            "{name:?} is not a name a {what} can have: lower-case letters, digits and `-`, starting with a letter"
        )),
    }
}

impl Manifest {
    /// Reads the manifest in a plugin's directory, and checks what Kito
    /// relies on.
    pub fn read(dir: &Path) -> Result<Manifest, String> {
        let path = dir.join(MANIFEST);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!("{}: {e} (a plugin has a {MANIFEST} next to its program)", path.display())
        })?;
        let manifest: Manifest = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        check_name("plugin", &manifest.name).map_err(|e| format!("{}: {e}", path.display()))?;
        for command in &manifest.commands {
            check_name("command", &command.name).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        for gate in &manifest.gates {
            check_name("gate", &gate.name).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        for tool in &manifest.tools {
            check_name("tool", &tool.name).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        if let Some(assistant) = &manifest.assistant {
            check_name("assistant", &assistant.name).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        for command in &manifest.server_commands {
            check_name("command", &command.name).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        if let Some(sign_in) = &manifest.sign_in {
            check_name("way to sign in", &sign_in.method).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        if manifest.api == 0 {
            return Err(format!(
                "{}: it does not say which API it was written for (`api = {}`)",
                path.display(),
                crate::rpc::API
            ));
        }
        Ok(manifest)
    }

    /// Why this daemon does not run it. `None`: it does.
    pub fn unfit(&self) -> Option<String> {
        (self.api > crate::rpc::API).then(|| {
            format!(
                "the plugin `{}` {} was written for API {}, and this Kito speaks API {}: update Kito",
                self.name,
                self.version,
                self.api,
                crate::rpc::API
            )
        })
    }
}

/// The program of a plugin, in its directory: `kito-<name>`.
pub fn program(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("kito-{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Where a person's plugins are installed: `plugins/` in their Kito
/// directory (`$KITO_CONFIG_DIR`, then `$XDG_CONFIG_HOME/kito`, then
/// `~/.config/kito`; on Windows `%APPDATA%\kito`).
pub fn plugins_dir() -> Option<PathBuf> {
    let non_empty = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = non_empty("KITO_CONFIG_DIR")
        .or_else(|| non_empty("XDG_CONFIG_HOME").map(|dir| dir.join("kito")))
        .or_else(|| non_empty("HOME").map(|home| home.join(".config").join("kito")))
        .or_else(|| non_empty("APPDATA").map(|dir| dir.join("kito")))?;
    Some(base.join("plugins"))
}

/// A plugin that is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub manifest: Manifest,
    pub dir: PathBuf,
}

/// The plugin of that name, if this person has it.
pub fn installed(name: &str) -> Option<Installed> {
    check_name("plugin", name).ok()?;
    let dir = plugins_dir()?.join(name);
    let manifest = Manifest::read(&dir).ok().filter(|manifest| manifest.name == name)?;
    Some(Installed { manifest, dir })
}

/// Every plugin this person has, by name.
pub fn all_installed() -> Vec<Installed> {
    let Some(dir) = plugins_dir() else { return Vec::new() };
    let mut found: Vec<Installed> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| installed(&entry.file_name().to_string_lossy()))
        .collect();
    found.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    found
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        match entry.file_type()?.is_dir() {
            true => copy_dir(&source, &target)?,
            // `copy` keeps the permissions: the program stays one.
            false => drop(std::fs::copy(&source, &target)?),
        }
    }
    Ok(())
}

/// Installs the plugin that is in a directory (its manifest and its
/// program), in the place of the version this person had.
pub fn install_from(source: &Path) -> Result<Installed, String> {
    let manifest = Manifest::read(source)?;
    if let Some(why) = manifest.unfit() {
        return Err(why);
    }
    let exe = program(source, &manifest.name);
    if !exe.is_file() {
        return Err(format!("{} is not there: the program of the plugin `{}`", exe.display(), manifest.name));
    }
    let plugins = plugins_dir().ok_or("no place to install plugins in: set KITO_CONFIG_DIR")?;
    let dir = plugins.join(&manifest.name);
    let staged = plugins.join(format!(".{}.new", manifest.name));
    let fail = |e: std::io::Error| format!("{}: {e}", dir.display());
    let _ = std::fs::remove_dir_all(&staged);
    copy_dir(source, &staged).map_err(fail)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| {
            format!(
                "{}: {e} (a plugin that is running holds its program: stop it, then again)",
                dir.display()
            )
        })?;
    }
    std::fs::rename(&staged, &dir).map_err(fail)?;
    Ok(Installed { manifest, dir })
}

/// Removes an installed plugin. `false`: there was none.
pub fn uninstall(name: &str) -> Result<bool, String> {
    check_name("plugin", name)?;
    let Some(dir) = plugins_dir().map(|dir| dir.join(name)).filter(|dir| dir.is_dir()) else {
        return Ok(false);
    };
    std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(true)
}

/// A plugin as a repository has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub enabled: bool,
    /// What the repository gave it.
    pub permissions: Vec<Permission>,
    /// Whether whoever runs the daemon has it.
    pub installed: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// What it asks for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asks: Vec<Permission>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Command>,
    /// The gates of integration it adds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<String>,
    /// The events it is run for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
    /// The tools it offers to agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    /// The coding assistant it sets up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<String>,
    /// It has something to say when a session starts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session: bool,
    /// What it last failed at, if it has not run well since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    /// Where its program is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<PathBuf>,
    /// Why it is not run, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// A plugin as a repository, or a directory connected to a server, has
/// it: what was said of it there, and what this person has installed.
pub fn info_of(name: &str, config: &PluginConfig) -> PluginInfo {
    let found = installed(name);
    let manifest = found.as_ref().map(|found| found.manifest.clone()).unwrap_or_default();
    let problem = match &found {
        None => Some(format!("the plugin `{name}` is not installed here: `kito plugin add {name}`")),
        Some(found) => found.manifest.unfit(),
    };
    PluginInfo {
        name: name.to_owned(),
        enabled: config.enabled,
        permissions: config.permissions.clone(),
        installed: found.is_some(),
        version: manifest.version,
        description: manifest.description,
        asks: manifest.permissions,
        gates: manifest.gates.iter().map(|gate| gate.name.clone()).collect(),
        events: manifest.events.iter().flat_map(|event| event.kinds.iter().cloned()).collect(),
        last_failure: None,
        tools: manifest.tools,
        assistant: manifest.assistant.map(|assistant| assistant.name),
        session: manifest.session.is_some(),
        commands: manifest.commands,
        program: found.map(|found| program(&found.dir, name)),
        problem,
    }
}

impl crate::Repo {
    /// The plugins this repository names, enabled or not.
    pub fn plugins(&self) -> Vec<PluginInfo> {
        self.config().plugins.iter().map(|(name, config)| info_of(name, config)).collect()
    }

    /// Enables a plugin here, with the permissions said, or else with
    /// what it asks for; never with more than it asks for.
    pub fn enable_plugin(
        &mut self,
        name: &str,
        permissions: Option<Vec<Permission>>,
    ) -> crate::Result<PluginInfo> {
        use crate::Error;
        check_name("plugin", name).map_err(Error::Other)?;
        let found = installed(name).ok_or_else(|| {
            Error::Other(format!("the plugin `{name}` is not installed: `kito plugin add` it first"))
        })?;
        if let Some(why) = found.manifest.unfit() {
            return Err(Error::Other(why));
        }
        // What a repository gives: of what it asks for, what is a repository's.
        let asked: Vec<Permission> = found
            .manifest
            .permissions
            .iter()
            .copied()
            .filter(|permission| permission.of_a_repository())
            .collect();
        let mut given = permissions.unwrap_or_else(|| asked.clone());
        if let Some(more) = given.iter().find(|permission| !asked.contains(permission)) {
            return Err(Error::Other(format!(
                "the plugin `{name}` does not ask for `{}`: it is given what it asks for, or less",
                more.word()
            )));
        }
        given.sort();
        given.dedup();
        let config = PluginConfig { enabled: true, permissions: given, for_clients: false };
        write_config(&self.root().join(kito_store::STORE_DIR), name, Some(&config)).map_err(Error::Config)?;
        ensure_key(self.root(), name).map_err(Error::io(self.root()))?;
        let info = info_of(name, &config);
        self.set_plugin_config(name, config);
        Ok(info)
    }

    /// Disables a plugin here: nothing of it runs, and its key opens
    /// nothing. `false`: it was not enabled.
    pub fn disable_plugin(&mut self, name: &str) -> crate::Result<bool> {
        let Some(mut config) = self.config().plugins.get(name).cloned().filter(|config| config.enabled)
        else {
            return Ok(false);
        };
        config.enabled = false;
        write_config(&self.root().join(kito_store::STORE_DIR), name, Some(&config))
            .map_err(crate::Error::Config)?;
        self.set_plugin_config(name, config);
        Ok(true)
    }

    /// The key an enabled plugin says to the daemon.
    pub fn plugin_key(&self, name: &str) -> crate::Result<String> {
        match self.config().plugins.get(name).is_some_and(|config| config.enabled) {
            true => ensure_key(self.root(), name).map_err(crate::Error::io(self.root())),
            false => Err(crate::Error::Other(format!(
                "the plugin `{name}` is not enabled in this repository: `kito plugin enable {name}`"
            ))),
        }
    }

    /// The plugins that run here: enabled, installed, and written for an
    /// API this daemon speaks.
    fn running(&self) -> Vec<Installed> {
        let enabled = self.config().plugins.iter().filter(|(_, config)| config.enabled);
        enabled
            .filter_map(|(name, _)| installed(name))
            .filter(|found| found.manifest.unfit().is_none())
            .collect()
    }

    /// The gates an integration asks, in the order of the plugins' names.
    pub fn gate_runs(&self) -> Vec<GateRun> {
        let mut gates = Vec::new();
        for found in self.running() {
            for gate in &found.manifest.gates {
                gates.push(GateRun {
                    plugin: found.manifest.name.clone(),
                    name: gate.name.clone(),
                    program: program(&found.dir, &found.manifest.name),
                    timeout: std::time::Duration::from_secs(gate.timeout_secs.unwrap_or(60).max(1)),
                });
            }
        }
        gates
    }

    /// The plugins that are run for events.
    pub fn event_runs(&self) -> Vec<EventRun> {
        let mut runs = Vec::new();
        for found in self.running() {
            let name = &found.manifest.name;
            let Ok(key) = ensure_key(self.root(), name) else { continue };
            for event in found.manifest.events.iter().filter(|event| !event.kinds.is_empty()) {
                runs.push(EventRun {
                    plugin: name.clone(),
                    program: program(&found.dir, name),
                    kinds: event.kinds.clone(),
                    timeout: std::time::Duration::from_secs(event.timeout_secs.unwrap_or(30).max(1)),
                    key: key.clone(),
                    root: self.root().to_owned(),
                });
            }
        }
        runs
    }

    /// What the gates of an integration are told: where the work lands,
    /// and each lane that is in the copy they look at, with its diff.
    pub fn gates_told(&self, lanes: &[kito_core::LaneId], target: kito_core::LaneId) -> serde_json::Value {
        let told: Vec<serde_json::Value> = lanes
            .iter()
            .map(|lane| {
                let info = self.lane_info(*lane).ok();
                let field = |get: fn(&crate::lanes::LaneInfo) -> Option<String>| info.as_ref().and_then(get);
                serde_json::json!({
                    "lane": lane.to_string(),
                    "agent": field(|info| Some(info.agent.clone())),
                    "owner": field(|info| info.owner.clone()),
                    "intent": field(|info| info.intent.clone()),
                    "task": field(|info| info.task.clone()),
                    "diff": self.lane_diff(*lane, &[]).ok(),
                })
            })
            .collect();
        serde_json::json!({ "api": crate::rpc::API, "target": crate::lanes::lane_word(target), "lanes": told })
    }

    /// Who said this key, among the plugins enabled here.
    pub fn plugin_of_key(&self, said: &str) -> Option<Access> {
        plugin_of_key(self.root(), said, &self.config().plugins)
    }
}

/// What a plugin keeps of its own in a repository: `.kito/plugins/<name>/`.
pub fn data_dir(root: &Path, name: &str) -> PathBuf {
    root.join(kito_store::STORE_DIR).join("plugins").join(name)
}

fn key_path(root: &Path, name: &str) -> PathBuf {
    data_dir(root, name).join("key")
}

/// The key a plugin says to the daemon of this repository, made the
/// first time. Readable by its owner only, like the socket's.
pub fn ensure_key(root: &Path, name: &str) -> std::io::Result<String> {
    use std::io::Write;
    let path = key_path(root, name);
    if let Some(key) =
        std::fs::read_to_string(&path).ok().map(|key| key.trim().to_owned()).filter(|k| k.len() == 64)
    {
        return Ok(key);
    }
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    let key: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // On Windows the user profile's permissions keep it from other users (ADR 0002).
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&path)?.write_all(key.as_bytes())?;
    Ok(key)
}

/// The plugin whose key this is, among those the repository enabled.
pub fn plugin_of_key(root: &Path, said: &str, enabled: &BTreeMap<String, PluginConfig>) -> Option<Access> {
    if said.len() != 64 {
        return None;
    }
    enabled.iter().filter(|(_, plugin)| plugin.enabled).find_map(|(name, plugin)| {
        let key = std::fs::read_to_string(key_path(root, name)).ok()?;
        // Compared whole, whatever differs: the time taken says nothing.
        let same = key.trim().len() == said.len()
            && key.trim().bytes().zip(said.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0;
        same.then(|| Access::Plugin { name: name.clone(), permissions: plugin.permissions.clone() })
    })
}

/// Writes what a repository says of a plugin in its configuration,
/// leaving everything else as it is written. `None` removes it.
pub fn write_config(store_dir: &Path, name: &str, plugin: Option<&PluginConfig>) -> Result<(), String> {
    let path = store_dir.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    if !doc.contains_key("plugins") {
        let mut plugins = toml_edit::Table::new();
        // `[plugins.web]`, with no empty `[plugins]` above it.
        plugins.set_implicit(true);
        doc["plugins"] = toml_edit::Item::Table(plugins);
    }
    let plugins = doc["plugins"]
        .as_table_mut()
        .ok_or_else(|| format!("{}: `plugins` is not a table", path.display()))?;
    match plugin {
        Some(plugin) => {
            let mut table = toml_edit::Table::new();
            table["enabled"] = toml_edit::value(plugin.enabled);
            let given: toml_edit::Array = plugin.permissions.iter().map(|p| p.word()).collect();
            table["permissions"] = toml_edit::value(given);
            plugins[name] = toml_edit::Item::Table(table);
        }
        None => drop(plugins.remove(name)),
    }
    kito_store::fs::atomic_write(&path, doc.to_string().as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_are_written_as_they_are_said() {
        let all = "[\"read\",\"write\",\"integrate\",\"admin\",\"sign_in\",\"pages\",\"server_events\",\"server_admin\"]";
        let read: Vec<Permission> = serde_json::from_str(all).unwrap();
        assert_eq!(read.len(), 8);
        assert_eq!(serde_json::to_string(&read).unwrap(), all);
        let words: Vec<String> = read.iter().map(|permission| format!("\"{}\"", permission.word())).collect();
        assert_eq!(format!("[{}]", words.join(",")), all);
    }

    #[test]
    fn each_method_needs_one_permission_and_unknown_ones_need_admin() {
        assert_eq!(permission_of("ping"), None);
        assert_eq!(permission_of("lane_diff"), Some(Permission::Read));
        assert_eq!(permission_of("comment"), Some(Permission::Write));
        assert_eq!(permission_of("request_integration"), Some(Permission::Integrate));
        assert_eq!(permission_of("configure"), Some(Permission::Admin));
        assert_eq!(permission_of("something_new"), Some(Permission::Admin));
        for method in READ.iter().chain(WRITE).chain(INTEGRATE) {
            let lists = [READ, WRITE, INTEGRATE].iter().filter(|list| list.contains(method)).count();
            assert_eq!(lists, 1, "{method}");
        }
    }

    #[test]
    fn a_plugin_is_answered_what_it_was_given() {
        let web =
            Access::Plugin { name: "web".into(), permissions: vec![Permission::Read, Permission::Write] };
        assert_eq!(web.refusal("ping"), None);
        assert_eq!(web.refusal("lanes"), None);
        assert_eq!(web.refusal("comment"), None);
        let said = web.refusal("request_integration").unwrap();
        assert!(
            said.contains("needs the permission `integrate`") && said.contains("kito plugin enable web"),
            "{said}"
        );
        // Not even with every permission: the keys of the others.
        let all = Access::Plugin {
            name: "web".into(),
            permissions: vec![Permission::Read, Permission::Write, Permission::Integrate, Permission::Admin],
        };
        assert!(all.refusal("plugin_key").unwrap().contains("not for plugins"));
        assert_eq!(all.refusal("configure"), None);
        assert_eq!(Access::Owner.refusal("plugin_key"), None);
    }

    #[test]
    fn a_manifest_says_what_a_plugin_is() {
        let dir = tempfile::tempdir().unwrap();
        let write = |text: &str| std::fs::write(dir.path().join(MANIFEST), text).unwrap();
        write(
            "name = \"web\"\nversion = \"0.1.0\"\ndescription = \"Kito in a browser\"\napi = 1\n\
             permissions = [\"read\", \"write\"]\n\n[[command]]\nname = \"web\"\nabout = \"Serve it\"\n",
        );
        let manifest = Manifest::read(dir.path()).unwrap();
        assert_eq!((manifest.name.as_str(), manifest.api), ("web", 1));
        assert_eq!(manifest.permissions, [Permission::Read, Permission::Write]);
        assert_eq!(manifest.commands[0].name, "web");
        assert_eq!(manifest.unfit(), None);

        write("name = \"web\"\nversion = \"9\"\napi = 99\n");
        assert!(Manifest::read(dir.path()).unwrap().unfit().unwrap().contains("update Kito"));
        write("name = \"web\"\n");
        assert!(Manifest::read(dir.path()).unwrap_err().contains("which API"));
        write("name = \"../web\"\napi = 1\n");
        assert!(Manifest::read(dir.path()).unwrap_err().contains("not a name a plugin can have"));
        write("name = \"web\"\napi = 1\npermissions = [\"everything\"]\n");
        assert!(Manifest::read(dir.path()).is_err());
        std::fs::remove_file(dir.path().join(MANIFEST)).unwrap();
        assert!(Manifest::read(dir.path()).unwrap_err().contains("kito-plugin.toml"));
    }

    #[test]
    fn a_repository_s_word_on_a_plugin_is_written_among_what_is_there() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "# Mine\n[verify]\ncommands = [\"true\"]\n").unwrap();
        let web = PluginConfig {
            enabled: true,
            permissions: vec![Permission::Read, Permission::Integrate],
            for_clients: false,
        };
        write_config(dir.path(), "web", Some(&web)).unwrap();
        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(text.starts_with("# Mine\n[verify]\ncommands = [\"true\"]\n"), "{text}");
        assert!(
            text.contains("[plugins.web]\nenabled = true\npermissions = [\"read\", \"integrate\"]\n"),
            "{text}"
        );
        assert!(!text.contains("[plugins]\n"), "{text}");
        let read: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(read.plugins["web"], web);
        write_config(dir.path(), "web", None).unwrap();
        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(!text.contains("web"), "{text}");
    }
}
