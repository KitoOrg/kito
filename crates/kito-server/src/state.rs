//! What a server keeps: its configuration, its people, its repositories.

use crate::audit::Lock;
use kito_core::paths::Canonical as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "server.toml";
/// In the environment of a plugin run for a server: the server's directory.
pub const ENV_SERVER_DIR: &str = "KITO_SERVER_DIR";
const REPOS_DIR: &str = "repos";
/// Where archived repositories wait to be deleted (ADR 0018).
const ARCHIVED_DIR: &str = "archived";
/// Who did what was done with `kito-server` on the server's machine.
pub const BY_HAND: &str = "the server's machine";

/// Days an archived repository is kept.
pub const KEEP_ARCHIVED_DAYS: u64 = 30;

/// Of the repositories a name may be (ADR 0018, amended): the one in
/// use, or the archived one. A repository added under the name of one
/// that is archived is another repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Live,
    Archived,
}
/// The address a new server listens on: this machine only. Clients on
/// other machines come in through SSH, or the address is changed by whoever
/// knows the network is theirs.
const DEFAULT_LISTEN: &str = "127.0.0.1:7421";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address `serve` listens on.
    pub listen: String,
    /// Address `serve` answers HTTP on (ADR 0006); none: it does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    pub users: BTreeMap<String, User>,
    /// The plugins this server enabled, by name (ADR 0006).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: BTreeMap<String, kitod::plugins::PluginConfig>,
    /// How people who sign in are kept.
    #[serde(skip_serializing_if = "SignInConfig::is_default")]
    pub sign_in: SignInConfig,
    /// Backups of the whole server (ADR 0017).
    #[serde(skip_serializing_if = "crate::backup::BackupConfig::is_default")]
    pub backup: crate::backup::BackupConfig,
    /// Repositories that were renamed, by their old name (ADR 0018): the
    /// directories connected with it still reach them.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub renamed: BTreeMap<String, String>,
}

/// `[sign_in]`: what holds for whoever signs in with a plugin (ADR 0006).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SignInConfig {
    /// Days a session lasts before the person signs in again.
    pub days: u64,
}

impl Default for SignInConfig {
    fn default() -> Self {
        SignInConfig { days: 30 }
    }
}

impl SignInConfig {
    fn is_default(&self) -> bool {
        *self == SignInConfig::default()
    }
}

/// Who a plugin says somebody is: nothing about what they may do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// `github`: the way to sign in of the plugin that said so.
    #[serde(default)]
    pub provider: String,
    /// What does not change when the person renames themselves there.
    pub subject: String,
    #[serde(default)]
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl Identity {
    /// As a user's configuration keeps it: `github:583231`.
    pub fn key(&self) -> String {
        format!("{}:{}", self.provider, self.subject)
    }

    /// As a person writes it, and reads it: `github:bob`.
    pub fn written(&self) -> String {
        format!("{}:{}", self.provider, self.login)
    }
}

/// Who an answer of a plugin says somebody is. The provider is the way to
/// sign in the plugin was enabled for, whatever it says: a plugin does
/// not speak for the people of another.
pub fn identity_of(method: &str, answer: &serde_json::Value) -> Result<Identity, String> {
    if let Some(why) = answer["refused"].as_str() {
        return Err(why.to_owned());
    }
    let mut identity: Identity = serde_json::from_value(answer["identity"].clone())
        .map_err(|_| "the plugin did not say who that is".to_owned())?;
    if identity.subject.trim().is_empty() {
        return Err("the plugin named nobody".to_owned());
    }
    identity.provider = method.to_owned();
    if identity.login.is_empty() {
        identity.login = identity.subject.clone();
    }
    Ok(identity)
}

/// The sessions of the people who signed in: what each token hashes to,
/// whose it is, and when it ends.
#[derive(Default, Serialize, Deserialize)]
struct Sessions {
    #[serde(default)]
    sessions: BTreeMap<String, Session>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    user: String,
    /// Milliseconds since the Unix epoch.
    until_ms: u64,
    /// What it was opened with (`a token`, `hello:bob`), when, and from
    /// which address if the server knew it: for the audit log to say
    /// through what a change was asked (ADR 0006, amended). None of it
    /// is a secret.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    with: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    opened_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    /// The link asked from this session, while its browser is away at the
    /// provider: it ends with the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link: Option<Link>,
}

/// A link that waits for its browser to come back (ADR 0006, amended).
/// The browser has a value nobody else has; what it hashes to is kept, so
/// that what is on the disk, and in every backup, brings nobody back.
#[derive(Clone, Serialize, Deserialize)]
struct Link {
    hash: String,
    until_ms: u64,
}

const SESSIONS_FILE: &str = "sessions.toml";
/// How many sessions a person has at most: opening one more ends their
/// oldest. The sessions are written whole at every change, and are not
/// one person's to make slow for everybody.
pub const SESSIONS_EACH: usize = 64;
/// How long a link waits for its browser, in seconds: as its cookie does.
pub const LINK_SECONDS: u64 = 600;
/// When each person was last here: a file each.
const SEEN_DIR: &str = "seen";
/// Who changed the settings of each repository, and to what: a file each.
const SETTINGS_DIR: &str = "settings";
/// How many changes of a repository's settings are kept.
const SETTINGS_KEPT: usize = 200;
/// What a session's token starts with: told apart from the tokens an
/// administrator hands out.
pub const SESSION_PREFIX: &str = "kitos_";
/// In the environment of a plugin that answers a client: who asks, about
/// which repository, and what they may do there.
pub const ENV_ASKED_BY: &str = "KITO_ASKED_BY";
pub const ENV_ASKED_REPO: &str = "KITO_ASKED_REPO";
pub const ENV_ASKED_ROLE: &str = "KITO_ASKED_ROLE";

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: DEFAULT_LISTEN.to_owned(),
            http: None,
            users: BTreeMap::new(),
            plugins: BTreeMap::new(),
            sign_in: SignInConfig::default(),
            backup: crate::backup::BackupConfig::default(),
            renamed: BTreeMap::new(),
        }
    }
}

/// What a person may do in a repository. Each role can do what the ones
/// before it can.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// See the lanes, the board and the history, and follow lanes.
    Read,
    /// Open lanes and work in them. A lane they ask to integrate is merged
    /// and checked, then waits for somebody who may integrate.
    Write,
    /// Integrate lanes, their own and those that wait; decide for any lane.
    #[default]
    Integrate,
    /// Change the repository's rules and who may use it (ADR 0014).
    Maintain,
}

impl Role {
    pub fn word(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Integrate => "integrate",
            Role::Maintain => "maintain",
        }
    }

    pub fn of_word(word: &str) -> Result<Role, String> {
        match word {
            "read" => Ok(Role::Read),
            "write" => Ok(Role::Write),
            "integrate" => Ok(Role::Integrate),
            "maintain" => Ok(Role::Maintain),
            other => Err(format!("`{other}` is no role in a repository: read, write, integrate or maintain")),
        }
    }
}

/// What a person may do on the server itself (ADR 0014).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum ServerRole {
    /// The repositories they were given.
    #[default]
    Member,
    /// Every repository, as `maintain`; people, and the server's plugins.
    Admin,
    /// As an admin, and names admins and owners.
    Owner,
}

impl ServerRole {
    pub fn word(self) -> &'static str {
        match self {
            ServerRole::Member => "member",
            ServerRole::Admin => "admin",
            ServerRole::Owner => "owner",
        }
    }

    pub fn of_word(word: &str) -> Result<ServerRole, String> {
        match word {
            "member" => Ok(ServerRole::Member),
            "admin" => Ok(ServerRole::Admin),
            "owner" => Ok(ServerRole::Owner),
            "read" | "write" | "integrate" | "maintain" => Err(format!(
                "`{word}` is a role in a repository: give it with --repo <repository>:{word}, or `grant`"
            )),
            other => Err(format!("`{other}` is no role on the server: member, admin or owner")),
        }
    }

    fn is_default(&self) -> bool {
        *self == ServerRole::default()
    }
}

/// A person who may use the server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The hash of their token: the token itself is nowhere on the server.
    pub token: String,
    /// What they may do on the server itself.
    #[serde(skip_serializing_if = "ServerRole::is_default")]
    pub server_role: ServerRole,
    /// The repositories they may use, and what they may do in each.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub access: BTreeMap<String, Role>,
    /// Before ADR 0014: the repositories (`*`: all), the role in them, and
    /// the roles that were something else. Read once, written no more.
    #[serde(skip_serializing)]
    pub repos: Vec<String>,
    #[serde(skip_serializing)]
    pub role: Role,
    #[serde(skip_serializing)]
    pub roles: BTreeMap<String, Role>,
    /// Who they are for the plugins that sign people in: `github:583231`
    /// (ADR 0006). The login it had when it was added is for people to read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub identities: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub logins: Vec<String>,
    /// Before ADR 0014: they could manage the server's plugins.
    #[serde(skip_serializing)]
    pub admin: bool,
}

impl User {
    pub fn may_use(&self, repo: &str) -> bool {
        self.role_in(repo).is_some()
    }

    /// What the person may do in a repository; `None`: they may not use it.
    pub fn role_in(&self, repo: &str) -> Option<Role> {
        match self.access.get(repo) {
            Some(role) => Some(*role),
            None => (self.server_role >= ServerRole::Admin).then_some(Role::Maintain),
        }
    }

    /// They manage the server: people, repositories given, plugins.
    pub fn is_admin(&self) -> bool {
        self.server_role >= ServerRole::Admin
    }

    /// They have a token: nothing hashes to nothing.
    pub fn has_token(&self) -> bool {
        !self.token.is_empty()
    }

    /// How they sign in, for people to read: `token`, then the login of
    /// each identity. Never a secret.
    pub fn signs_in_with(&self) -> Vec<String> {
        let token = self.has_token().then(|| "token".to_owned());
        token.into_iter().chain(self.logins.iter().cloned()).collect()
    }

    /// Who they are for a way to sign in, by the login it had when it was
    /// attached; `None`: they have no identity of that way.
    pub fn login_of(&self, method: &str) -> Option<String> {
        let kept = self.identities.iter().find(|kept| of_method(kept, method))?;
        Some(self.logins.iter().find(|login| of_method(login, method)).unwrap_or(kept).clone())
    }

    /// What was written before ADR 0014, as it was meant: `true` if
    /// anything was. `all`: the repositories the server has.
    fn read_as_meant(&mut self, all: &[String]) -> bool {
        if self.repos.is_empty() && self.roles.is_empty() && !self.admin {
            return false;
        }
        for repo in std::mem::take(&mut self.repos) {
            let repos = match repo == "*" {
                true => all.to_vec(),
                false => vec![repo],
            };
            for repo in repos {
                let role = self.roles.get(&repo).copied().unwrap_or(self.role);
                self.access.entry(repo).or_insert(role);
            }
        }
        self.roles.clear();
        if std::mem::take(&mut self.admin) {
            self.server_role = self.server_role.max(ServerRole::Admin);
        }
        true
    }
}

/// `true` if an identity or a login, kept as `github:…`, is of this way
/// to sign in.
fn of_method(kept: &str, method: &str) -> bool {
    kept.split_once(':').is_some_and(|(way, _)| way == method)
}

pub struct Server {
    pub dir: PathBuf,
    pub config: Config,
    /// What `server.toml` hashed to when this was read from it, or last
    /// wrote it; none: there was none yet. It is written over only while
    /// it is still that (`save`).
    read: Option<String>,
    /// What must hold for a change to be made through this (`only_while`).
    only_while: Option<Still>,
}

/// Whether whoever a change is made for may still ask for it, by the
/// server as it was just read; `Err`: they may not, and why.
type Still = Box<dyn Fn(&Server) -> Result<(), String> + Send + Sync>;

/// One change of the server's configuration, being made (ADR 0014,
/// amended): the lock is held, and the configuration is the one that was
/// on the disk when it was taken. What a change checks, it checks here,
/// on that; and nothing slow is done here: no plugin is asked, nothing is
/// cloned, nothing goes over the network.
///
/// Whatever writes is a method of this, so that nothing is written by a
/// `Server` that does not hold the lock, and no change is begun inside
/// another: what is read through it is the server, and cannot be changed
/// through that.
pub struct Change<'a> {
    server: &'a mut Server,
    held: &'a Lock,
    /// Whose sessions it ended, and how many: they are ended whether or
    /// not the change is then written.
    ended: Vec<(String, usize)>,
    /// What is to be told of it once it is written: each kind, what the
    /// audit log keeps, and what the plugins are told.
    told: Vec<(&'static str, serde_json::Value, serde_json::Value)>,
}

impl std::ops::Deref for Change<'_> {
    type Target = Server;

    fn deref(&self) -> &Server {
        self.server
    }
}

/// What a token is kept as.
pub fn hash_of(token: &str) -> String {
    kito_core::Hash::of(token.as_bytes()).to_string()
}

/// What a token, a session and a link are made of: 32 bytes nobody has
/// yet, in hex.
fn random_hex(what: &str) -> Result<String, String> {
    let mut bytes = [0u8; 32];
    // The system's randomness, on every platform (ADR 0002).
    getrandom::getrandom(&mut bytes).map_err(|e| format!("no randomness to make {what} from: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Where a file is staged before it is put in place: beside it, under a
/// name no other writer has, in this process or another
/// (`server.toml.<pid>-<n>.new`).
fn staged_beside(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STAGED: AtomicU64 = AtomicU64::new(0);
    let nth = STAGED.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{name}.{}-{nth}.new", std::process::id()))
}

/// Writes a file readable by its owner only: what tokens hash to, what
/// sessions hash to. On Windows the user profile's permissions keep it
/// from other users (ADR 0002).
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).and_then(|mut file| file.write_all(text.as_bytes()))
}

/// Puts a file in place whole, by way of a staged one of its own, under
/// the lock of its directory (`.lock` there): for what is written beside
/// the configuration and not by a change of it (when people were last
/// here, the history of settings). `change` is given what the file says
/// now and answers what it says after; `None`: nothing is written.
/// `wait`: the lock is waited for as any; without, one held by another
/// is not, and nothing is written.
fn rewrite(
    dir: &Path,
    name: &str,
    wait: bool,
    change: impl FnOnce(String) -> Option<String>,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let held = match wait {
        true => Lock::take(dir),
        false => Lock::take_now(dir),
    };
    let _held = held.map_err(|failed| match failed {
        None => "held by something that does not finish".to_owned(),
        Some(e) => e.to_string(),
    })?;
    let path = dir.join(name);
    let Some(text) = change(std::fs::read_to_string(&path).unwrap_or_default()) else { return Ok(()) };
    // Hidden: no person and no repository has a name that starts with a dot.
    let staged = staged_beside(&path);
    let staged = dir.join(format!(".{}", staged.file_name().unwrap_or_default().to_string_lossy()));
    let written = std::fs::write(&staged, text).and_then(|()| std::fs::rename(&staged, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written.map_err(|e| format!("{}: {e}", path.display()))
}

/// A token for a person, which nobody has yet.
fn new_token() -> Result<String, String> {
    Ok(format!("kito_{}", random_hex("a token")?))
}

/// What whoever cannot be checked is told: on the clients' port, a
/// connection that cannot be held to the configuration as it is now is
/// not held to an earlier one.
pub const UNREADABLE: &str = "the server's configuration cannot be read just now: try again";

/// What a writer is told when `server.toml` is no longer the file it
/// read: it writes nothing, rather than undo somebody's change.
const CHANGED_MEANWHILE: &str =
    "the server's configuration was changed meanwhile: nothing was written; ask again";

/// Says a line where whoever runs the server reads, as `eprintln!` does,
/// and goes on when it cannot: the standard error of a server run for one
/// client goes when its SSH connection does, and nothing the server does
/// after that stops on it.
pub fn say(line: std::fmt::Arguments) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "kito-server: {line}");
}

/// What whoever asked is told when the server's own files fail it: why,
/// which names where the server is, goes to its standard error only.
pub const COULD_NOT: &str = "the server could not do this just now: its log says why";

/// Says why the server could not do something, where whoever runs it
/// reads, and answers what whoever asked is told (`COULD_NOT`).
pub fn could_not(why: std::fmt::Arguments) -> String {
    say(why);
    COULD_NOT.to_owned()
}

/// What the plugins are told, in the order it happened: the server's
/// directory, a number of its own, the kind and the event.
type Later = (PathBuf, u64, String, serde_json::Value);

/// What is not told yet, by its number: its kind, for `told` to say what
/// it gave up waiting for.
fn waiting() -> &'static std::sync::Mutex<BTreeMap<u64, String>> {
    static WAITING: std::sync::OnceLock<std::sync::Mutex<BTreeMap<u64, String>>> = std::sync::OnceLock::new();
    WAITING.get_or_init(Default::default)
}

/// Takes an event off what waits when it is dropped: whatever happened
/// while it was told, a panic too, whoever waits (`told`) is not left
/// waiting for it.
struct Told(u64);

impl Drop for Told {
    fn drop(&mut self) {
        waiting().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&self.0);
    }
}

/// Where they wait: one thread tells them, one at a time and in order.
fn later() -> &'static std::sync::Mutex<std::sync::mpsc::Sender<Later>> {
    static LATER: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Later>>> =
        std::sync::OnceLock::new();
    LATER.get_or_init(|| {
        let (sender, waiting) = std::sync::mpsc::channel::<Later>();
        std::thread::spawn(move || {
            for (dir, nth, kind, event) in waiting {
                let _told = Told(nth);
                // The plugins as they are when it is told, not when it
                // happened. One that goes wrong is said, and the next is
                // told all the same.
                let telling = std::panic::catch_unwind(|| match Server::read(&dir) {
                    Ok((server, _)) => server.tell_plugins(&kind, event),
                    Err(why) => say(format_args!("no plugin was told of `{kind}`: {why}")),
                });
                if telling.is_err() {
                    say(format_args!("no plugin was told of `{kind}`: telling them failed"));
                }
            }
        });
        std::sync::Mutex::new(sender)
    })
}

/// How long `told` waits in all: four times as long as a plugin is let
/// run for an event when it does not ask for more (`timeout_secs`).
const TOLD_WITHIN: std::time::Duration = std::time::Duration::from_secs(120);

/// Waits until everything that was to be told has been: for a process
/// that leaves once it has done what it was asked. Gives up after
/// `TOLD_WITHIN` in all, and says what it gave up on.
pub fn told() {
    told_within(TOLD_WITHIN);
}

fn told_within(within: std::time::Duration) {
    let left = || waiting().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let since = std::time::Instant::now();
    loop {
        let now = left();
        if now.is_empty() {
            return;
        }
        if since.elapsed() >= within {
            let kinds: Vec<String> = now.into_values().collect();
            return say(format_args!(
                "the plugins were not told of {} after {} s, and are not waited for: {}",
                kinds.len(),
                within.as_secs(),
                kinds.join(", ")
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A name that is safe as a directory and in a configuration key.
fn check_name(what: &str, name: &str) -> Result<(), String> {
    let plain = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        && !name.starts_with('.');
    match plain {
        true => Ok(()),
        false => Err(format!(
            "{name:?} is not a name a {what} can have: letters, digits, `-`, `_` and `.`, not starting with a dot"
        )),
    }
}

/// How many files a repository may be given to start with, and how many
/// bytes each may hold (ADR 0018, amended).
pub const FIRST_FILES: usize = 32;
pub const FIRST_FILE_BYTES: usize = 256 * 1024;

/// The files a new repository starts with: for each, its path in the
/// repository and its text. These were checked: every path stays inside
/// the repository and out of Kito's own directory and git's.
pub struct FirstFiles(Vec<(String, String)>);

impl FirstFiles {
    /// Checks the paths and texts given; what cannot be is said with its path.
    pub fn new(files: &[(&str, &str)]) -> Result<FirstFiles, String> {
        if files.len() > FIRST_FILES {
            return Err(format!(
                "a repository starts with at most {FIRST_FILES} files, and {} were given: the rest goes in \
                 through a lane",
                files.len()
            ));
        }
        let mut given = std::collections::BTreeSet::new();
        for (path, text) in files {
            if let Some(why) = not_a_first_path(path) {
                return Err(format!("{path:?} is not a path inside the repository: {why}"));
            }
            if text.len() > FIRST_FILE_BYTES {
                return Err(format!(
                    "{path:?} is too large to start a repository with: {} bytes, and a file is at most \
                     {FIRST_FILE_BYTES} (256 KiB)",
                    text.len()
                ));
            }
            if !given.insert(*path) {
                return Err(format!("{path:?} is given twice"));
            }
        }
        // A path is a file or a directory, not both.
        for path in &given {
            let mut above = path.match_indices('/').map(|(at, _)| &path[..at]);
            if let Some(file) = above.find(|dir| given.contains(dir)) {
                return Err(format!("{path:?} is given under {file:?}, which is given as a file"));
            }
        }
        Ok(FirstFiles(files.iter().map(|(path, text)| ((*path).to_owned(), (*text).to_owned())).collect()))
    }
}

/// Why a file of a new repository cannot have this path; `None`: it can.
/// A path is written as Kito keeps them: from the repository's
/// directory, with `/` between its parts.
fn not_a_first_path(path: &str) -> Option<String> {
    if path.is_empty() {
        return Some("it is empty".into());
    }
    if path.starts_with('/') {
        return Some("it starts with `/`, and a path starts from the repository's directory".into());
    }
    if path.contains('\\') {
        return Some("its parts are separated with `/`, not `\\`".into());
    }
    if path.chars().any(char::is_control) {
        return Some("it has a control character in it".into());
    }
    for part in path.split('/') {
        match part {
            "" => return Some("it has an empty part (`//`, or a `/` at its end)".into()),
            "." => return Some("it has `.` in it, which names nothing".into()),
            ".." => return Some("it has `..` in it, which leads out of the repository".into()),
            _ => {}
        }
        // What this machine reads as more than a name: `C:` on Windows.
        let mut read = Path::new(part).components();
        let name = matches!(
            (read.next(), read.next()),
            (Some(std::path::Component::Normal(name)), None) if name == part
        );
        if !name {
            return Some(format!("this machine does not read {part:?} as a name"));
        }
    }
    // Whatever the case: on some disks `.GIT` is `.git`.
    let first = path.split('/').next().unwrap_or_default();
    let kept = [(kito_store::STORE_DIR, "Kito"), (".git", "git")];
    let (own, whose) = kept.into_iter().find(|(own, _)| first.eq_ignore_ascii_case(own))?;
    Some(format!("`{own}` is {whose}'s own directory"))
}

impl Server {
    pub fn init(dir: &Path) -> Result<Server, String> {
        std::fs::create_dir_all(dir.join(REPOS_DIR)).map_err(|e| format!("{}: {e}", dir.display()))?;
        let dir = dir.canonical().map_err(|e| format!("{}: {e}", dir.display()))?;
        if dir.join(CONFIG_FILE).exists() {
            return Err(format!("{} is a server already", dir.display()));
        }
        let mut server = Server { dir, config: Config::default(), read: None, only_while: None };
        let held = server.hold()?;
        server.save(&held)?;
        Ok(server)
    }

    /// The server as `server.toml` has it now, with what it was read from
    /// remembered. `true`: it was written before ADR 0014, and is read as
    /// it was meant, in memory only (`open` writes it back).
    fn read(dir: &Path) -> Result<(Server, bool), String> {
        let dir = dir.canonical().map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(CONFIG_FILE);
        let text = std::fs::read_to_string(&path).map_err(|_| {
            format!("{} is not a Kito server (run `kito-server init` there, or give --dir)", dir.display())
        })?;
        let config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut server = Server { dir, config, read: Some(hash_of(&text)), only_while: None };
        let all = server.repos();
        let mut old = false;
        for user in server.config.users.values_mut() {
            old |= user.read_as_meant(&all);
        }
        Ok((server, old))
    }

    pub fn open(dir: &Path) -> Result<Server, String> {
        let (mut server, old) = Server::read(dir)?;
        // Written before ADR 0014: read as it was meant, written back so.
        if old {
            server.change(|_| Ok::<(), String>(()))?;
        }
        Ok(server)
    }

    /// The server as `server.toml` has it now, for a request from afar.
    /// When it cannot be read, why is said on this machine, where it
    /// names the server's directory, and whoever asked is told only that.
    pub fn open_for_a_request(dir: &Path) -> Result<Server, String> {
        Server::open(dir).map_err(|why| {
            say(format_args!("{why}"));
            UNREADABLE.to_owned()
        })
    }

    /// Holds what the server keeps of who may come in, its configuration
    /// and its sessions, for one writer at a time across its processes.
    /// One lock for both: a change of one often ends or reads the other,
    /// and with one there is no order to take two in. It is taken once
    /// for a change, by `change` or by what writes the sessions alone;
    /// what they call is given it. When it cannot be taken that is said,
    /// never passed over: to whoever asked, who may be far away, so
    /// without where the server is.
    fn hold(&self) -> Result<Lock, String> {
        Lock::take(&self.dir).map_err(|failed| match failed {
            None => {
                "the server's configuration and its sessions are held by something that does not finish: \
                     nothing was changed; ask again in a moment"
                    .to_owned()
            }
            Some(e) => format!("the server's lock cannot be taken ({e}): nothing was changed"),
        })
    }

    /// From here on, a change is made through this only while `still`
    /// holds: asked first in every change, of the configuration it has
    /// just read. For a server opened to answer somebody's request: what
    /// they came with may have been taken away since the request began,
    /// and what they are may have changed, however long the request took
    /// (a clone, a plugin asked) before it changes anything.
    pub fn only_while(&mut self, still: impl Fn(&Server) -> Result<(), String> + Send + Sync + 'static) {
        self.only_while = Some(Box::new(still));
    }

    /// Makes one change of the configuration, alone (ADR 0014, amended):
    /// the lock is taken, `server.toml` is read again, `change` checks and
    /// changes what was just read, and that is written. Whoever read the
    /// server before, however long ago, writes over nothing that was
    /// changed since. `Err`: nothing is written.
    ///
    /// What is slow (a plugin asked, a clone, the network) is done before
    /// this, and checked again in `change`.
    pub fn change<T, E: From<String>>(
        &mut self,
        change: impl FnOnce(&mut Change) -> Result<T, E>,
    ) -> Result<T, E> {
        self.change_and(change, |_, _| ()).map(|(made, ())| made)
    }

    /// Makes one change, and tells what `change` answers to tell with its
    /// lock still held (`Change::then_tell`): for whoever builds the
    /// events from what the change made.
    pub fn change_told<T>(
        &mut self,
        change: impl FnOnce(&mut Change) -> Result<(T, Vec<(&'static str, serde_json::Value)>), String>,
    ) -> Result<T, String> {
        self.change(|making| {
            let (made, told) = change(making)?;
            making.then_tell_all(told);
            Ok(made)
        })
    }

    /// Makes one change as `change` does, and then `and`, with the lock
    /// still held, once what the change made is written: what must follow
    /// the change and come before any other (a repository's directory
    /// moved once nobody is given it any more).
    ///
    /// What the change is to tell (`Change::then_tell`) takes its place in
    /// the audit log and in what the plugins are told with the lock held
    /// too: in the order the changes were made, whoever made them.
    fn change_and<T, E: From<String>, R>(
        &mut self,
        change: impl FnOnce(&mut Change) -> Result<T, E>,
        and: impl FnOnce(&Server, &T) -> R,
    ) -> Result<(T, R), E> {
        self.change_then(change, and, |_, _| None)
    }

    /// `change_and`, and `undo` with the lock still held when what the
    /// change made could not be written: what it did outside the
    /// configuration (a directory moved) is put back before anybody else
    /// changes anything. `undo` answers what could not be put back, said
    /// after why nothing was written.
    fn change_then<T, E: From<String>, R>(
        &mut self,
        change: impl FnOnce(&mut Change) -> Result<T, E>,
        and: impl FnOnce(&Server, &T) -> R,
        undo: impl FnOnce(&Server, &T) -> Option<String>,
    ) -> Result<(T, R), E> {
        let held = self.hold()?;
        let old = Server::read(&self.dir)
            .map(|(now, old)| {
                (self.config, self.read) = (now.config, now.read);
                old
            })
            .map_err(|why| {
                say(format_args!("{why}"));
                UNREADABLE.to_owned()
            })?;
        if let Some(still) = &self.only_while {
            still(self)?;
        }
        let before = self.config.clone();
        let mut making = Change { server: &mut *self, held: &held, ended: Vec::new(), told: Vec::new() };
        let made = change(&mut making);
        let (ended, told) = (making.ended, making.told);
        let written = match &made {
            Ok(_) if old || self.config != before => self.save(&held),
            _ => Ok(()),
        };
        let made = match (made, written) {
            (Ok(made), Ok(())) => made,
            // What was not written is not what this goes on with. Sessions
            // it ended stay ended: that is kept and told, and said to
            // whoever asked, who would read that nothing was done.
            (made, written) => {
                self.config = before;
                let events = ended.iter().flat_map(|(name, ended)| Server::sessions_ended(name, *ended));
                self.tell_after(events.collect());
                let left = match &made {
                    Ok(made) => undo(self, made),
                    Err(_) => None,
                };
                drop(held);
                // Made, then: it is what was written that failed.
                let why = written.err().unwrap_or_default();
                return Err(made.err().unwrap_or_else(|| {
                    let why = match left {
                        Some(left) => format!("{why}; {left}"),
                        None => why,
                    };
                    E::from(match Self::already_ended(&ended) {
                        Some(already) => format!("{why}; {already}"),
                        None => why,
                    })
                }));
            }
        };
        #[cfg(test)]
        tests::pausing(&self.dir, &told);
        for (kind, kept, _) in &told {
            self.note(kind, kept);
        }
        self.queue(told.into_iter().map(|(kind, _, event)| (kind.to_owned(), event)).collect());
        let after = and(self, &made);
        drop(held);
        Ok((made, after))
    }

    /// `3 sessions of bob had already ended`, for each person a change
    /// ended sessions of before it failed; `None`: it ended none.
    fn already_ended(ended: &[(String, usize)]) -> Option<String> {
        let said: Vec<String> = ended
            .iter()
            .filter(|(_, ended)| *ended > 0)
            .map(|(name, ended)| match ended {
                1 => format!("1 session of {name} had already ended"),
                more => format!("{more} sessions of {name} had already ended"),
            })
            .collect();
        (!said.is_empty()).then(|| said.join(", "))
    }

    /// Writes the configuration: it holds what tokens hash to, and is
    /// readable by its owner only. For whoever holds the lock, and only
    /// over what this was read from: a writer that did not read again
    /// (none is left) writes nothing, rather than undo somebody's change.
    fn save(&mut self, _held: &Lock) -> Result<(), String> {
        #[cfg(test)]
        tests::failing(&self.dir, "saving").map_err(|why| could_not(format_args!("{why}")))?;
        let path = self.dir.join(CONFIG_FILE);
        let text = toml::to_string(&self.config).map_err(|e| e.to_string())?;
        // Staged in a file of its own, then put in place whole: nobody
        // reads half of one.
        let staged = staged_beside(&path);
        let written = write_private(&staged, &text)
            .map_err(|e| could_not(format_args!("{}: {e}", staged.display())))
            // Asked last, right before it is put in place: whether the
            // file is the one it read.
            .and_then(|()| {
                let on_disk = std::fs::read_to_string(&path).ok().map(|text| hash_of(&text));
                match on_disk == self.read {
                    true => Ok(()),
                    false => Err(CHANGED_MEANWHILE.to_owned()),
                }
            })
            .and_then(|()| {
                std::fs::rename(&staged, &path)
                    .map_err(|e| could_not(format_args!("{}: {e}", path.display())))
            });
        if written.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        written?;
        self.read = Some(hash_of(&text));
        Ok(())
    }

    /// Takes out what a server that was stopped in the middle of a write
    /// left staged beside `server.toml` and `sessions.toml`: they hold
    /// what tokens hash to. For when the server starts; one written just
    /// now is another's. And what is left of repositories deleted, of
    /// repositories made aside and never given their name, and of dates of
    /// archived ones staged.
    pub fn sweep_staged(&self) {
        let (minute, day) = (std::time::Duration::from_secs(60), std::time::Duration::from_secs(86_400));
        let entries = |dir: PathBuf| std::fs::read_dir(dir).into_iter().flatten().flatten();
        let older = |entry: &std::fs::DirEntry, than: std::time::Duration| {
            let age = entry.metadata().and_then(|meta| meta.modified()).ok().and_then(|at| at.elapsed().ok());
            age.is_some_and(|age| age > than)
        };
        let named = |entry: &std::fs::DirEntry| entry.file_name().to_string_lossy().into_owned();
        for entry in entries(self.dir.clone()) {
            let name = named(&entry);
            let beside =
                [CONFIG_FILE, SESSIONS_FILE].iter().any(|kept| name.starts_with(&format!("{kept}.")));
            if beside && name.ends_with(".new") && older(&entry, minute) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        for entry in entries(self.dir.join(REPOS_DIR)).chain(entries(self.archived_dir())) {
            let name = named(&entry);
            // Moved aside once its deletion was written, whatever its age:
            // a directory moved keeps the time it had.
            let deleted = name.starts_with(".deleted-");
            // Made aside and never given its name: a clone of another
            // process takes its time, and is not swept from under it.
            let abandoned = name.starts_with(".adding-") && older(&entry, day);
            let staged = name.contains(".json.") && name.ends_with(".new") && older(&entry, minute);
            if deleted || abandoned {
                let _ = std::fs::remove_dir_all(entry.path());
            } else if staged {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Where a repository is, if the server has it.
    pub fn repo(&self, name: &str) -> Option<PathBuf> {
        check_name("repository", name).ok()?;
        let root = self.dir.join(REPOS_DIR).join(name);
        root.join(kito_store::STORE_DIR).join("format").is_file().then_some(root)
    }

    pub fn repos(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.join(REPOS_DIR))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| self.repo(name).is_some())
            .collect();
        names.sort();
        names
    }

    // ------------------------------------------------- repositories (ADR 0018)

    /// The name a repository has now: one renamed is still reached by its
    /// old name.
    pub fn resolve(&self, name: &str) -> String {
        let mut now = name.to_owned();
        for _ in 0..16 {
            if self.repo(&now).is_some() {
                break;
            }
            match self.config.renamed.get(&now) {
                Some(next) => now = next.clone(),
                None => break,
            }
        }
        now
    }

    /// Stops the daemon of a repository, if one runs, and waits for it.
    fn stop_daemon(root: &Path) {
        if let Ok(mut client) = kitod::client::Client::connect(root) {
            let _ = client.call_value("shutdown", serde_json::json!({}));
            let socket = kitod::rpc::socket_path(root);
            for _ in 0..1000 {
                if !socket.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    fn archived_dir(&self) -> PathBuf {
        self.dir.join(ARCHIVED_DIR)
    }

    /// Takes a repository out of use: hidden and closed, kept for
    /// `KEEP_ARCHIVED_DAYS`, then deleted. Who may use it stays said, for
    /// when it comes back.
    ///
    /// Its directory is moved with the lock held, as a repository is
    /// renamed and deleted: of two that move the same name at once, one
    /// moves it and the other finds it moved.
    pub fn archive_repo(&self, name: &str, by: Option<&str>) -> Result<(), String> {
        let places = |server: &Server| -> Result<(PathBuf, PathBuf), String> {
            let root =
                server.repo(name).ok_or_else(|| format!("there is no repository `{name}` on this server"))?;
            let to = server.archived_dir().join(name);
            match to.exists() {
                true => Err(format!("an archived repository is called `{name}` already: delete it first")),
                false => Ok((root, to)),
            }
        };
        // Asked before its daemon is stopped, which takes a while, and
        // again where it is moved.
        let (root, _) = places(self)?;
        Self::stop_daemon(&root);
        let _held = self.hold()?;
        let (root, to) = places(self)?;
        std::fs::create_dir_all(self.archived_dir()).map_err(|e| e.to_string())?;
        // When and by whom is in place before the directory is moved: an
        // archived repository whose date is missing would be due at once.
        self.date_archived(name, now_ms(), by.unwrap_or(BY_HAND))?;
        if let Err(e) = std::fs::rename(&root, &to) {
            let _ = std::fs::remove_file(self.archived_said(name));
            return Err(could_not(format_args!("{}: {e}", root.display())));
        }
        // Kept and queued with the lock held: in the order it happened.
        let what = format!("{name}, deleted in {KEEP_ARCHIVED_DAYS} days");
        self.tell("repo_archived", serde_json::json!({ "repo": name, "by": by, "what": what }));
        Ok(())
    }

    /// Where it is said when a repository was archived, and by whom.
    fn archived_said(&self, name: &str) -> PathBuf {
        self.archived_dir().join(format!("{name}.json"))
    }

    /// Writes when an archived repository was archived and by whom, whole:
    /// staged under a name of its own, then put in place. For whoever
    /// holds the lock.
    fn date_archived(&self, name: &str, at_ms: u64, by: &str) -> Result<(), String> {
        let path = self.archived_said(name);
        let staged = staged_beside(&path);
        let said = serde_json::json!({ "at_ms": at_ms, "by": by });
        let written =
            std::fs::write(&staged, said.to_string()).and_then(|()| std::fs::rename(&staged, &path));
        if written.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        written.map_err(|e| could_not(format_args!("{}: {e}", path.display())))
    }

    /// When an archived repository was archived, if that can be read.
    fn archived_at(&self, name: &str) -> Option<(u64, serde_json::Value)> {
        let text = std::fs::read_to_string(self.archived_said(name)).ok()?;
        let said: serde_json::Value = serde_json::from_str(&text).ok()?;
        Some((said["at_ms"].as_u64()?, said["by"].clone()))
    }

    /// Whether an archived repository was kept as long as one is kept. One
    /// whose date cannot be read is never due.
    fn archived_due(&self, name: &str) -> bool {
        self.archived_at(name).is_some_and(|(at, _)| at + KEEP_ARCHIVED_DAYS * 86_400_000 <= now_ms())
    }

    /// The archived repositories: name, when, by whom, and when it goes.
    /// One whose date cannot be read says none, and is not due.
    pub fn archived(&self) -> Vec<serde_json::Value> {
        let mut found: Vec<serde_json::Value> = std::fs::read_dir(self.archived_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                // What is being taken away (`.deleted-…`) is no repository.
                check_name("repository", &name).ok()?;
                let said = self.archived_at(&name);
                let at = said.as_ref().map(|(at, _)| *at);
                Some(serde_json::json!({
                    "name": name,
                    "at_ms": at,
                    "by": said.map(|(_, by)| by),
                    "deleted_at_ms": at.map(|at| at + KEEP_ARCHIVED_DAYS * 86_400_000),
                }))
            })
            .collect();
        found.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        found
    }

    /// Brings an archived repository back, as it was.
    pub fn unarchive_repo(&self, name: &str, by: Option<&str>) -> Result<(), String> {
        check_name("repository", name)?;
        // Moved with the lock held: not from under the purge that deletes it.
        let _held = self.hold()?;
        let from = self.archived_dir().join(name);
        if !from.is_dir() {
            return Err(format!("no archived repository is called `{name}`"));
        }
        let to = self.dir.join(REPOS_DIR).join(name);
        if to.exists() {
            return Err(format!("the server has a repository `{name}` already: rename one first"));
        }
        std::fs::rename(&from, &to).map_err(|e| could_not(format_args!("{}: {e}", from.display())))?;
        let _ = std::fs::remove_file(self.archived_dir().join(format!("{name}.json")));
        // Kept and queued with the lock held: in the order it happened.
        self.tell("repo_unarchived", serde_json::json!({ "repo": name, "by": by, "what": name }));
        Ok(())
    }

    /// Which repository of a name is meant, when one in use and an
    /// archived one may have it (ADR 0018, amended): the one `asked`, or
    /// the only one there is. Both there and none said is refused: what
    /// is asked of one is never done to the other.
    pub fn which_repo(&self, name: &str, asked: Option<Which>) -> Result<Which, String> {
        check_name("repository", name)?;
        let (live, kept) = (self.repo(name).is_some(), self.archived_dir().join(name).is_dir());
        match (asked, live, kept) {
            (Some(Which::Live), true, _) | (None, true, false) => Ok(Which::Live),
            (Some(Which::Archived), _, true) | (None, false, true) => Ok(Which::Archived),
            (None, true, true) => Err(format!(
                "the server has a repository `{name}` and an archived one of that name: say which is to be \
                 deleted (--archived or --live; in a request, `archived`: true or false)"
            )),
            (Some(Which::Archived), _, false) => Err(format!("no archived repository is called `{name}`")),
            (_, false, _) => Err(format!("there is no repository `{name}` on this server")),
        }
    }

    /// Deletes one repository for good, the one in use or the archived one
    /// of that name (`which_repo`), and nothing else. The backups still
    /// have it. Answers which it was. `by`: who asked, none on the
    /// server's machine.
    ///
    /// It is one deletion, under one lock: which repository it is; what
    /// the people were given of it and its old names, written; then its
    /// directory moved aside, beside where it was (`repos/.deleted-…`,
    /// `archived/.deleted-…`: on the same disk), and what is left of its
    /// name taken away. The lock is let go and the directory removed,
    /// which may take long. Nothing is moved back: a directory moved
    /// aside is always one whose deletion was written, and what the
    /// server sweeps when it starts.
    ///
    /// Once no repository in use has the name, nothing is left of it:
    /// nobody is given it, its history of settings goes, and no
    /// repository has it as its kitchen. A repository added later under
    /// the name starts with nothing of this one.
    pub fn delete_repo(
        &mut self,
        name: &str,
        asked: Option<Which>,
        by: Option<&str>,
    ) -> Result<Which, String> {
        self.delete_repo_when(name, asked, by, false)
    }

    /// `delete_repo`; with `due`, only an archived one kept its time, as
    /// it is when the lock is held: the purge's list is made before, and
    /// another may have been archived under the name meanwhile.
    fn delete_repo_when(
        &mut self,
        name: &str,
        asked: Option<Which>,
        by: Option<&str>,
        due: bool,
    ) -> Result<Which, String> {
        check_name("repository", name)?;
        let place = |server: &Server| -> Result<(Which, PathBuf), String> {
            match server.which_repo(name, asked)? {
                Which::Live => {
                    let root = server.repo(name);
                    root.map(|root| (Which::Live, root))
                        .ok_or_else(|| format!("there is no repository `{name}`"))
                }
                Which::Archived if due && !server.archived_due(name) => {
                    Err(format!("the archived repository `{name}` is not due to be deleted"))
                }
                Which::Archived => Ok((Which::Archived, server.archived_dir().join(name))),
            }
        };
        if let Ok((Which::Live, root)) = place(self) {
            Self::stop_daemon(&root);
        }
        let aside_name = format!(".deleted-{name}-{}", random_hex("a name")?.split_at(16).0);
        let deleted = self.change_and(
            |change| {
                let (which, root) = place(change)?;
                if which == Which::Live || change.repo(name).is_none() {
                    let config = change.config_mut();
                    for user in config.users.values_mut() {
                        user.access.remove(name);
                    }
                    config.renamed.retain(|old, new| old != name && new != name);
                }
                Ok::<_, String>((which, root))
            },
            // Written: its directory is moved aside, and what is left of
            // its name taken away, before anybody else changes anything.
            |server, (which, root)| -> Result<PathBuf, String> {
                let aside = root.with_file_name(&aside_name);
                #[cfg(test)]
                let failing = tests::failing(&server.dir, "moving-aside");
                #[cfg(not(test))]
                let failing = Ok(());
                if let Err(e) =
                    failing.and_then(|()| std::fs::rename(root, &aside).map_err(|e| e.to_string()))
                {
                    say(format_args!("{} was not moved to {}: {e}", root.display(), aside.display()));
                    let what =
                        format!("{name}: nobody is given it any more, and its directory is still there");
                    server.note(
                        "repo_not_deleted",
                        &serde_json::json!({ "repo": name, "by": by, "what": what }),
                    );
                    return Err(e);
                }
                if *which == Which::Archived {
                    let _ = std::fs::remove_file(server.archived_said(name));
                }
                if server.repo(name).is_none() {
                    server.forget_name(name);
                }
                let what = match (which, due) {
                    (Which::Live, _) => name.to_owned(),
                    (Which::Archived, false) => format!("{name}, archived"),
                    (Which::Archived, true) => format!("{name}, archived {KEEP_ARCHIVED_DAYS} days ago"),
                };
                server.tell("repo_deleted", serde_json::json!({ "repo": name, "by": by, "what": what }));
                Ok(aside)
            },
        );
        let ((which, _), moved) = deleted?;
        let aside = moved.map_err(|why| {
            format!(
                "`{name}` is given to nobody any more, but its directory could not be moved ({why}): ask for \
                 the deletion again"
            )
        })?;
        // Deleted: what is left to take away is the sweep's when it fails.
        #[cfg(test)]
        let removed = tests::failing(&self.dir, "removing")
            .and_then(|()| std::fs::remove_dir_all(&aside).map_err(|e| e.to_string()));
        #[cfg(not(test))]
        let removed = std::fs::remove_dir_all(&aside);
        if let Err(e) = removed {
            say(format_args!("{} is deleted, and not yet removed: {e}", aside.display()));
        }
        Ok(which)
    }

    /// Takes away what is left of a name no repository has any more: its
    /// history of settings, and its place as the kitchen of others. What
    /// fails is said, and undoes nothing.
    fn forget_name(&self, name: &str) {
        let history = self.dir.join(SETTINGS_DIR).join(format!("{name}.jsonl"));
        if let Err(e) = std::fs::remove_file(&history) {
            if e.kind() != std::io::ErrorKind::NotFound {
                say(format_args!("{}: {e}", history.display()));
            }
        }
        let path = format!("../{name}");
        for other in self.repos() {
            let Some(root) = self.repo(&other) else { continue };
            let store = root.join(kito_store::STORE_DIR);
            let names_it = kitod::config::Config::load(&store)
                .is_ok_and(|config| config.kitchen.path.as_deref() == Some(path.as_str()));
            if names_it {
                Self::stop_daemon(&root);
                if let Err(why) = kitod::config::write_value(&store, "kitchen", "path", None) {
                    say(format_args!("`{other}` has the deleted `{name}` as its kitchen still: {why}"));
                }
            }
        }
    }

    /// Deletes the archived repositories kept longer than they are kept:
    /// each the archived one, never one in use that has its name since.
    /// One archived with no date it can read is dated now, and has its
    /// days from then. Answers which were deleted.
    pub fn purge_archived(&mut self) -> Vec<String> {
        let now = now_ms();
        let mut due = Vec::new();
        for one in self.archived() {
            let Some(name) = one["name"].as_str().map(str::to_owned) else { continue };
            match one["deleted_at_ms"].as_u64() {
                Some(at) if at <= now => due.push(name),
                Some(_) => {}
                None => match self.hold().and_then(|_held| match self.archived_at(&name) {
                    // Dated meanwhile, by whoever archived it.
                    Some(_) => Ok(false),
                    None => self.date_archived(&name, now, "the server").map(|()| true),
                }) {
                    Ok(false) => {}
                    Ok(true) => say(format_args!(
                        "the archived `{name}` said not when it was archived: it is dated now, and deleted in \
                         {KEEP_ARCHIVED_DAYS} days"
                    )),
                    Err(why) => say(format_args!("the archived `{name}` could not be dated: {why}")),
                },
            }
        }
        due.into_iter()
            .filter(|name| self.delete_repo_when(name, Some(Which::Archived), None, true).is_ok())
            .collect()
    }

    /// Renames a repository: who may use it, the kitchens that name it and
    /// what was kept of its settings follow; the old name still reaches it.
    pub fn rename_repo(&mut self, from: &str, to: &str, by: Option<&str>) -> Result<(), String> {
        check_name("repository", to)?;
        // Whether it can be, on the disk as it is: asked before its daemon
        // is stopped, which takes a while, and again where it is renamed.
        let places = |server: &Server| -> Result<(PathBuf, PathBuf), String> {
            let root =
                server.repo(from).ok_or_else(|| format!("there is no repository `{from}` on this server"))?;
            let target = server.dir.join(REPOS_DIR).join(to);
            match target.exists() || server.archived_dir().join(to).exists() {
                true => Err(format!("the server has a repository `{to}` already")),
                false => Ok((root, target)),
            }
        };
        let (root, _) = places(self)?;
        Self::stop_daemon(&root);
        // Its directory and what the configuration says of it, in one
        // change; a directory moved for nothing is moved back, with the
        // lock still held: nobody has taken the old name by then.
        let renamed = self.change_then(
            |change| {
                let (root, target) = places(change)?;
                std::fs::rename(&root, &target)
                    .map_err(|e| could_not(format_args!("{}: {e}", root.display())))?;
                let config = change.config_mut();
                for user in config.users.values_mut() {
                    if let Some(role) = user.access.remove(from) {
                        user.access.insert(to.to_owned(), role);
                    }
                }
                // An old name taken by the new one is no longer an old name.
                config.renamed.remove(to);
                for new in config.renamed.values_mut() {
                    if new == from {
                        *new = to.to_owned();
                    }
                }
                config.renamed.insert(from.to_owned(), to.to_owned());
                let what = format!("{from} is {to} now");
                change.then_tell("repo_renamed", serde_json::json!({ "repo": to, "by": by, "what": what }));
                Ok::<_, String>((root, target))
            },
            |_, _| (),
            |server, (root, target)| {
                #[cfg(test)]
                let failing = tests::failing(&server.dir, "moving-back");
                #[cfg(not(test))]
                let failing = Ok(());
                let back = failing.and_then(|()| std::fs::rename(target, root).map_err(|e| e.to_string()));
                let e = back.err()?;
                say(format_args!("{} was not moved back to {}: {e}", target.display(), root.display()));
                let _ = server;
                Some(format!(
                    "its directory is still called `{to}`, and could not be called `{from}` again ({e})"
                ))
            },
        );
        renamed?;
        let settings = self.dir.join("settings");
        let _ = std::fs::rename(settings.join(format!("{from}.jsonl")), settings.join(format!("{to}.jsonl")));
        // The repositories whose kitchen it is.
        let old_path = format!("../{from}");
        for name in self.repos() {
            let Some(other) = self.repo(&name) else { continue };
            let store = other.join(kito_store::STORE_DIR);
            let names_it = kitod::config::Config::load(&store)
                .is_ok_and(|config| config.kitchen.path.as_deref() == Some(old_path.as_str()));
            if names_it {
                Self::stop_daemon(&other);
                let new_path = format!("../{to}");
                kitod::config::write_value(&store, "kitchen", "path", Some(new_path.as_str().into()))?;
            }
        }
        Ok(())
    }

    /// Adds a repository from a git address, with a token used for the
    /// clone and kept nowhere.
    pub fn add_repo_from_git(
        &self,
        name: &str,
        url: &str,
        token: Option<&str>,
        told: Vec<(&'static str, serde_json::Value)>,
    ) -> Result<PathBuf, String> {
        let token = token.map(str::trim).filter(|token| !token.is_empty());
        let given = match token {
            Some(token) => match url.split_once("://") {
                Some(("https", rest)) => format!("https://x-access-token:{token}@{rest}"),
                _ => return Err("a token goes with an https:// address".into()),
            },
            None => url.to_owned(),
        };
        let added = self.add_repo_told(name, Some(&given), None, true, told).map_err(|why| match token {
            Some(token) => why.replace(token, "<token>"),
            None => why,
        })?;
        // The clone keeps where it came from, without the token.
        if token.is_some() {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&added)
                .args(["remote", "set-url", "origin", url])
                .output();
        }
        Ok(added)
    }

    /// Where a repository sent from a client is put together while it comes.
    pub fn upload_path(&self, id: &str) -> Result<PathBuf, String> {
        let plain = id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit());
        match plain {
            true => Ok(self.dir.join("uploads").join(id)),
            false => Err("no such upload".into()),
        }
    }

    /// Creates a repository: with the history of a git one, or with a
    /// first version made of the files given, a `README.md` when none are
    /// (ADR 0018, amended). One that comes with a history is protected
    /// unless told otherwise: its main lane changes only when a lane is
    /// integrated.
    #[cfg(test)] // What tells nothing is for the tests: a change that tells does it under its lock.
    pub fn add_repo(
        &self,
        name: &str,
        from: Option<&str>,
        files: Option<&FirstFiles>,
        protect: bool,
    ) -> Result<PathBuf, String> {
        self.add_repo_told(name, from, files, protect, Vec::new())
    }

    /// `add_repo`, telling these once it has its name, with the lock that
    /// gave it held (`take_name`).
    ///
    /// Nothing is built under its name: the repository is made aside
    /// (`adding_dir`), and takes its name when it is whole and closed. What
    /// fails takes away the directory made aside, and nothing else.
    pub fn add_repo_told(
        &self,
        name: &str,
        from: Option<&str>,
        files: Option<&FirstFiles>,
        protect: bool,
        told: Vec<(&'static str, serde_json::Value)>,
    ) -> Result<PathBuf, String> {
        check_name("repository", name)?;
        if from.is_some() && files.is_some() {
            return Err("a repository comes from a git one or from the files given, not both".into());
        }
        // Said at once when the name is taken: nothing is cloned for
        // nothing. Taken for good only when the repository is whole.
        self.name_free(name)?;
        let aside = self.adding_dir(name)?;
        let made = match from {
            Some(from) => Self::clone_repo(name, &aside, from, protect),
            None => {
                // An empty main lane has nothing a lane could start from.
                let readme = FirstFiles(vec![("README.md".to_owned(), format!("# {name}\n"))]);
                let files = files.filter(|files| !files.0.is_empty()).unwrap_or(&readme);
                Self::start_repo(&aside, files).map_err(|why| format!("{name}: {why}"))
            }
        };
        if let Err(why) = made {
            let _ = std::fs::remove_dir_all(&aside);
            return Err(why);
        }
        self.take_name(name, &aside, told)
    }

    /// Whether a repository could be given this name now: nothing in
    /// `repos/` has it, whatever it is, and no archived repository does.
    pub fn name_free(&self, name: &str) -> Result<(), String> {
        check_name("repository", name)?;
        if self.dir.join(REPOS_DIR).join(name).exists() {
            return Err(format!("the server already has something called `{name}`"));
        }
        // A name is one repository's, an archived one's too: what people
        // were given of it is said under the name, and would be theirs of
        // whatever came to be called so (ADR 0018, amended).
        if self.archived_dir().join(name).exists() {
            return Err(format!(
                "an archived repository is called `{name}`: bring it back, delete it, or give this one \
                 another name"
            ));
        }
        Ok(())
    }

    /// An empty directory to make the repository `name` in, nobody else's:
    /// `repos/.adding-<name>-<random>`. No name of a repository starts
    /// with a dot, so nothing made there is one until it takes its name.
    pub fn adding_dir(&self, name: &str) -> Result<PathBuf, String> {
        check_name("repository", name)?;
        let repos = self.dir.join(REPOS_DIR);
        let aside = repos.join(format!(".adding-{name}-{}", random_hex("a name")?.split_at(16).0));
        std::fs::create_dir_all(&repos)
            .and_then(|()| std::fs::create_dir(&aside))
            .map_err(|e| could_not(format_args!("{}: {e}", aside.display())))?;
        Ok(aside)
    }

    /// Gives the repository made in `made` (`adding_dir`) its name, with
    /// the lock held: refused when anything has the name by then, else
    /// moved into place, and `told` kept and queued before the lock is let
    /// go. Whatever fails, `made` is taken away, and nothing else.
    pub fn take_name(
        &self,
        name: &str,
        made: &Path,
        told: Vec<(&'static str, serde_json::Value)>,
    ) -> Result<PathBuf, String> {
        let root = self.dir.join(REPOS_DIR).join(name);
        let taken = (|| -> Result<PathBuf, String> {
            let _held = self.hold()?;
            self.name_free(name)?;
            std::fs::rename(made, &root).map_err(|e| could_not(format_args!("{}: {e}", made.display())))?;
            self.tell_after(told);
            Ok(root)
        })();
        if taken.is_err() {
            let _ = std::fs::remove_dir_all(made);
        }
        taken
    }

    /// A message of something the server did in its own directory, as
    /// whoever asked from afar is told it: where it is said on this
    /// machine, and not in the message.
    pub fn without_dir(&self, why: String) -> String {
        let dir = self.dir.display().to_string();
        match why.contains(&dir) {
            true => {
                say(format_args!("{why}"));
                why.replace(&dir, "<the server's directory>")
            }
            false => why,
        }
    }

    /// Makes the repository `name` of the empty directory `root` from a
    /// git one: cloned there, with its history imported.
    fn clone_repo(name: &str, root: &Path, from: &str, protect: bool) -> Result<(), String> {
        // A directory here is given to git as a plain path: git on
        // Windows does not take one that starts with `\\?\` (ADR 0002).
        let local = Path::new(from).canonical().ok().filter(|path| path.is_dir());
        let from = local.as_ref().and_then(|path| path.to_str()).unwrap_or(from);
        let cloned = std::process::Command::new("git")
            .args(["clone", "--quiet", from])
            .arg(root)
            .output()
            .map_err(|e| format!("git: {e}"))?;
        if !cloned.status.success() {
            let said = String::from_utf8_lossy(&cloned.stderr);
            return Err(format!("git clone {from}: {}", said.trim()));
        }
        // Kito's own directory is not git's business.
        let exclude = root.join(".git").join("info").join("exclude");
        let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
        text.push_str("\n.kito/\n");
        let _ = std::fs::write(&exclude, text);
        let said = |e: kitod::Error| format!("{name}: {e}");
        let mut repo = kitod::Repo::init(root).map_err(said)?;
        repo.import_git().map_err(said)?;
        repo.scan().map_err(said)?;
        if protect {
            repo.protect(true).map_err(said)?;
        }
        repo.close().map_err(said)
    }

    /// Makes a repository of the empty directory `root`, with its first
    /// version: the files written in its main directory, recorded, and
    /// in a first checkpoint.
    fn start_repo(root: &Path, files: &FirstFiles) -> Result<(), String> {
        let said = |e: kitod::Error| e.to_string();
        // The files first: what they say is ignored (`.gitignore`,
        // `.kitoignore`) holds from the start, as in any directory.
        for (path, text) in &files.0 {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap_or(root))
                .and_then(|()| std::fs::write(&file, text))
                .map_err(|e| format!("{path}: {e}"))?;
        }
        let mut repo = kitod::Repo::init(root).map_err(said)?;
        repo.scan().map_err(said)?;
        // What is not recorded would be on the server's disk and on no lane.
        if let Some((path, _)) = files.0.iter().find(|(path, _)| !repo.tree().contains(Path::new(path))) {
            return Err(format!(
                "{path:?} would not be in the first version: the files given leave it out (a `.gitignore`, a \
                 `.kitoignore`, or a `.git` in its path)"
            ));
        }
        repo.checkpoint("first version").map_err(said)?;
        repo.close().map_err(said)
    }

    /// Adds a person and returns their token, which is not kept. With an
    /// identity they sign in instead, and have no token.
    #[cfg(test)] // What tells nothing is for the tests: a change that tells does it under its lock.
    pub fn add_user(
        &mut self,
        name: &str,
        email: Option<String>,
        access: &[(String, Role)],
        server_role: ServerRole,
        identity: Option<&Identity>,
    ) -> Result<Option<String>, String> {
        self.change(|change| change.add_user(name, email, access, server_role, identity))
    }

    /// Notes that a person was here now: at most once a minute each, in
    /// `seen/<name>` (one file each, written whole).
    pub fn saw(&self, name: &str) {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        static NOTED: OnceLock<Mutex<HashMap<String, std::time::Instant>>> = OnceLock::new();
        let noted = NOTED.get_or_init(|| Mutex::new(HashMap::new()));
        {
            let mut noted = noted.lock().unwrap_or_else(|e| e.into_inner());
            if noted.get(name).is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(60)) {
                return;
            }
            noted.insert(name.to_owned(), std::time::Instant::now());
        }
        if check_name("user", name).is_err() {
            return;
        }
        // Best done, on whatever thread asks: noted if nobody holds the
        // lock now, and else at a later request, never waited for.
        let noted = rewrite(&self.dir.join(SEEN_DIR), name, false, |_| Some(now_ms().to_string()));
        if noted.is_err() {
            let noted = NOTED.get_or_init(|| Mutex::new(HashMap::new()));
            noted.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
        }
    }

    /// Notes that `by` set a repository's `setting` to `value`: read,
    /// changed and written by one writer at a time, so that two changes
    /// at once are both kept. One that cannot be noted is said.
    pub fn note_setting(&self, repo: &str, by: &str, setting: &str, value: &serde_json::Value) {
        if check_name("repository", repo).is_err() {
            return;
        }
        let line = serde_json::json!({ "at_ms": now_ms(), "by": by, "setting": setting, "value": value });
        let noted = rewrite(&self.dir.join(SETTINGS_DIR), &format!("{repo}.jsonl"), true, |text| {
            let mut lines: Vec<&str> = text.lines().collect();
            let line = line.to_string();
            lines.push(&line);
            let keep = lines.len().saturating_sub(SETTINGS_KEPT);
            Some(lines[keep..].join("\n") + "\n")
        });
        if let Err(why) = noted {
            say(format_args!(
                "the change of `{setting}` in {repo} by {by} was not kept in its history: {why}"
            ));
        }
    }

    /// The changes of a repository's settings, the latest first.
    pub fn settings_history(&self, repo: &str) -> Vec<serde_json::Value> {
        let path = self.dir.join(SETTINGS_DIR).join(format!("{repo}.jsonl"));
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut changes: Vec<serde_json::Value> =
            text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
        changes.reverse();
        changes
    }

    /// When a person was last here, in milliseconds since 1970.
    pub fn seen(&self, name: &str) -> Option<u64> {
        std::fs::read_to_string(self.dir.join(SEEN_DIR).join(name)).ok()?.trim().parse().ok()
    }

    /// Changes what a person may do on the server itself.
    #[cfg(test)] // What tells nothing is for the tests: a change that tells does it under its lock.
    pub fn set_server_role(&mut self, name: &str, role: ServerRole) -> Result<(), String> {
        self.change(|change| change.set_server_role(name, role))
    }

    /// Gives a person a repository, with a role in it; `None` takes it
    /// away. Answers whether anything changed.
    pub fn grant(&mut self, name: &str, repo: &str, role: Option<Role>) -> Result<bool, String> {
        self.change(|change| change.grant(name, repo, role))
    }

    /// Who may use a repository, and as what: those given it, and the
    /// admins.
    pub fn members(&self, repo: &str) -> Vec<(String, Role, bool)> {
        self.config
            .users
            .iter()
            .filter_map(|(name, user)| {
                let given = user.access.contains_key(repo);
                user.role_in(repo).map(|role| (name.clone(), role, !given))
            })
            .collect()
    }

    /// Removes a person, and their sessions with them.
    #[cfg(test)] // What tells nothing is for the tests: a change that tells does it under its lock.
    pub fn remove_user(&mut self, name: &str) -> Result<(), String> {
        self.change(|change| change.remove_user(name))
    }

    // ------------------------------- how a person signs in (ADR 0006, amended)
    //
    // Each of these is one change, made alone (`change`). The same four
    // are methods of a change, for whoever has more to check where it is
    // made: who asks, and whether they still may.

    /// Attaches an identity to a person, who keeps what they signed in
    /// with. `false`: it was theirs already, and nothing changed.
    #[cfg(test)] // What tells nothing is for the tests: a change that tells does it under its lock.
    pub fn link(&mut self, name: &str, identity: &Identity) -> Result<bool, String> {
        self.change(|change| change.link(name, identity))
    }

    /// Detaches a person's identity of a way to sign in, and ends their
    /// sessions but `keep`. Answers the login it was, and how many
    /// sessions ended.
    #[cfg(test)] // What tells nothing is for the tests: a change that tells does it under its lock.
    pub fn unlink(
        &mut self,
        name: &str,
        method: &str,
        keep: Option<&str>,
    ) -> Result<(String, usize), String> {
        self.change(|change| change.unlink(name, method, keep))
    }

    /// Gives a person a new token and returns it, which is not kept;
    /// whether they had one, and how many sessions ended with it.
    #[cfg(test)] // What tells nothing is for the tests: a change that tells does it under its lock.
    pub fn change_token(&mut self, name: &str, keep: Option<&str>) -> Result<(String, bool, usize), String> {
        self.change(|change| change.change_token(name, keep))
    }

    /// A new token and nothing else to sign in with, for a token that
    /// leaked (`Change::token_alone`), as one change.
    #[cfg(test)] // What tells nothing is for the tests: a change that tells does it under its lock.
    pub fn token_alone(
        &mut self,
        name: &str,
        keep: Option<&str>,
    ) -> Result<(String, Vec<String>, bool, usize), String> {
        self.change(|change| change.token_alone(name, keep))
    }

    /// What is kept and told of a token given alone: `token_changed`,
    /// which names the identities taken away; then `identity_unlinked`
    /// for each, and `session_ended` for each session that ended. `by`:
    /// who asked, none on the server's machine.
    pub fn alone_events(
        name: &str,
        by: Option<&str>,
        (unlinked, replaced): (&[String], bool),
        ended: usize,
    ) -> Vec<(&'static str, serde_json::Value)> {
        let what = match unlinked.is_empty() {
            true => format!("{name}, alone: no identity to unlink"),
            false => format!("{name}, alone: {} unlinked", unlinked.join(", ")),
        };
        let changed = serde_json::json!({
            "user": name, "replaced": replaced, "alone": true, "unlinked": unlinked, "by": by, "what": what,
        });
        let each = unlinked.iter().map(|login| {
            let event = serde_json::json!({ "user": name, "by": by, "what": format!("{name}: {login}") });
            ("identity_unlinked", event)
        });
        std::iter::once(("token_changed", changed))
            .chain(each)
            .chain(Self::sessions_ended(name, ended))
            .collect()
    }

    /// What is kept and told of a change that ended sessions: the change,
    /// then each session that ended as `end_session` tells its own.
    pub fn with_sessions_ended(
        change: (&'static str, serde_json::Value),
        name: &str,
        ended: usize,
    ) -> Vec<(&'static str, serde_json::Value)> {
        std::iter::once(change).chain(Self::sessions_ended(name, ended)).collect()
    }

    /// One `session_ended` for each of so many sessions of a person.
    fn sessions_ended(name: &str, ended: usize) -> Vec<(&'static str, serde_json::Value)> {
        (0..ended).map(|_| ("session_ended", serde_json::json!({ "user": name, "what": name }))).collect()
    }

    // ---------------------------------------------------------- signing in

    /// The user an identity is, if the server has one.
    pub fn user_by_identity(&self, identity: &Identity) -> Option<String> {
        let key = identity.key();
        self.config
            .users
            .iter()
            .find(|(_, user)| user.identities.contains(&key))
            .map(|(name, _)| name.clone())
    }

    /// The ways to sign in: the plugins enabled for it, by their method.
    pub fn sign_in_methods(&self) -> Vec<(String, String)> {
        let running = self.running();
        let may = running
            .iter()
            .filter(|(_, config)| config.permissions.contains(&kitod::plugins::Permission::SignIn));
        may.filter_map(|(found, _)| found.manifest.sign_in.as_ref())
            .map(|spec| (spec.method.clone(), spec.about.clone()))
            .collect()
    }

    /// Why a way to sign in cannot be used from a browser now (`sign-in
    /// ready`), or `None`: it can, or its plugin does not know the
    /// question, and is taken to be ready as it always was.
    pub fn sign_in_not_ready(&self, method: &str) -> Option<String> {
        let answer = self.sign_in_step(method, &["ready"], &serde_json::json!({ "kind": "browser" })).ok()?;
        (answer["ready"] == false).then(|| answer["why"].as_str().unwrap_or("it is not set up").to_owned())
    }

    /// Asks the plugin that signs people in by `method` one thing
    /// (`begin`, `finish`, `ready`, `lookup <login>`), with `told` on its
    /// input.
    pub fn sign_in_step(
        &self,
        method: &str,
        step: &[&str],
        told: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        use kitod::plugins::Permission;
        let running = self.running();
        let plugin = running.iter().find(|(found, config)| {
            config.permissions.contains(&Permission::SignIn)
                && found.manifest.sign_in.as_ref().is_some_and(|spec| spec.method == method)
        });
        let Some((found, _)) = plugin else {
            let known: Vec<String> = self.sign_in_methods().into_iter().map(|(method, _)| method).collect();
            return Err(match known.is_empty() {
                true => format!("this server has no way to sign in with `{method}`: no plugin enabled here signs people in"),
                false => format!("this server has no way to sign in with `{method}`: it has {}", known.join(", ")),
            });
        };
        let name = &found.manifest.name;
        let program = kitod::plugins::program(&found.dir, name);
        let mut args = vec!["sign-in"];
        args.extend(step);
        let envs = [(ENV_SERVER_DIR, self.dir.as_os_str().to_owned())];
        let input = serde_json::to_vec(told).unwrap_or_default();
        let printed = kitod::plugins::run_program(
            &program,
            &args,
            &self.dir,
            &envs,
            &input,
            std::time::Duration::from_secs(60),
        )
        .map_err(|why| format!("the plugin `{name}` failed at `sign-in {}`: {why}", step[0]))?;
        let text = String::from_utf8_lossy(&printed);
        let last = text.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or_default();
        serde_json::from_str(last)
            .map_err(|_| format!("the plugin `{name}` answered something that is not an answer"))
    }

    /// The person a plugin says somebody is, by a login: for `user add`.
    pub fn look_up(&self, identity: &str) -> Result<Identity, String> {
        let (method, login) = identity
            .split_once(':')
            .filter(|(method, login)| !method.is_empty() && !login.is_empty())
            .ok_or("an identity is written <method>:<login>, as in github:bob")?;
        let answer = self.sign_in_step(method, &["lookup", login], &serde_json::json!({ "login": login }))?;
        identity_of(method, &answer).map_err(|why| format!("{identity}: {why}"))
    }

    /// Who an identity is on this server, told to the plugins that
    /// listen; `Err`: nobody, and why.
    pub fn signed_in(&self, identity: &Identity) -> Result<String, String> {
        let who = identity.written();
        match self.user_by_identity(identity) {
            Some(name) => {
                self.tell("signed_in", serde_json::json!({ "user": name, "identity": identity.key(), "what": format!("{name} ({who})") }));
                Ok(name)
            }
            None => {
                self.tell("sign_in_refused", serde_json::json!({ "identity": identity.key(), "what": who }));
                Err(Self::nobody(identity))
            }
        }
    }

    fn sessions(&self) -> Sessions {
        std::fs::read_to_string(self.dir.join(SESSIONS_FILE))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Changes the sessions, for whoever holds the lock, and writes them,
    /// readable by the server's user only; the ones that ended are
    /// dropped on the way. Answers what `change` does.
    fn write_sessions<T>(
        &self,
        _held: &Lock,
        change: impl FnOnce(&mut BTreeMap<String, Session>) -> T,
    ) -> Result<T, String> {
        let mut all = self.sessions();
        let now = now_ms();
        all.sessions.retain(|_, session| session.until_ms > now);
        let made = change(&mut all.sessions);
        let path = self.dir.join(SESSIONS_FILE);
        let text = toml::to_string(&all).map_err(|e| e.to_string())?;
        // Staged under a name of its own, as `server.toml` is.
        let staged = staged_beside(&path);
        #[cfg(test)]
        if self.dir.join(tests::UNWRITABLE).exists() {
            return Err(could_not(format_args!("{}: the tests keep it from being written", path.display())));
        }
        let written = write_private(&staged, &text).and_then(|()| std::fs::rename(&staged, &path));
        if written.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        written.map_err(|e| could_not(format_args!("{}: {e}", path.display())))?;
        Ok(made)
    }

    /// Opens a session for a user and returns its token, which is not
    /// kept: it ends after `[sign_in] days`.
    ///
    /// `theirs` says, of the server as it is on the disk when the session
    /// is written, whether what they proved who they are with is theirs
    /// still; `None`: it is not, and no session opens. Asked with the
    /// lock held, as every change is made: this comes before a change of
    /// how they sign in, which ends the session, or after it, and finds
    /// it made.
    ///
    /// A person has `SESSIONS_EACH` sessions at most: one more ends their
    /// oldest.
    ///
    /// `with` is what it is opened with (`a token`, `hello:bob`) and
    /// `from` the address it is asked from, if the server knows it: kept
    /// with it, for the audit log to say through what a change was asked.
    pub fn open_session(
        &self,
        name: &str,
        (with, from): (&str, Option<&str>),
        theirs: impl FnOnce(&Server) -> bool,
    ) -> Result<Option<String>, String> {
        let token = format!("{SESSION_PREFIX}{}", random_hex("a session")?);
        let new = hash_of(&token);
        let held = self.hold()?;
        let (now, _) = Server::read(&self.dir).map_err(|_| UNREADABLE.to_owned())?;
        if !theirs(&now) {
            return Ok(None);
        }
        let opened_ms = now_ms();
        let until_ms = opened_ms + now.config.sign_in.days * 24 * 3600 * 1000;
        let session = Session {
            user: name.to_owned(),
            until_ms,
            with: with.to_owned(),
            opened_ms,
            from: from.map(str::to_owned),
            link: None,
        };
        let ended = self.write_sessions(&held, |sessions| {
            sessions.insert(new.clone(), session);
            let mut ended = 0;
            while sessions.values().filter(|session| session.user == name).count() > SESSIONS_EACH {
                // Of the ones they had, the one that ends first: their
                // oldest, while a session lasts as long as it did.
                let had = sessions.iter().filter(|(hash, session)| session.user == name && **hash != new);
                let oldest = had.min_by_key(|(_, session)| session.until_ms).map(|(hash, _)| hash.clone());
                let Some(oldest) = oldest else { break };
                sessions.remove(&oldest);
                ended += 1;
            }
            ended
        })?;
        // Kept and queued with the lock held: in the order it happened.
        self.tell_after(Self::sessions_ended(name, ended));
        drop(held);
        Ok(Some(token))
    }

    /// How a session came to be, for the audit log: `opened with a token
    /// on 2026-11-23 10:12:05 UTC`, and the address it was opened from
    /// apart (`203.0.113.7`), which the plugins are not told. `None`: there
    /// is no such session, or it was opened before sessions said so.
    pub fn session_opened(&self, hash: &str) -> Option<(String, Option<String>)> {
        let session = self.sessions().sessions.get(hash).cloned()?;
        if session.with.is_empty() {
            return None;
        }
        let at = crate::audit::moment_of(session.opened_ms);
        Some((format!("opened with {} on {at}", session.with), session.from))
    }

    /// Begins a link for the session a browser is signed in with (ADR
    /// 0006, amended): a value for it to bring back, which is not kept.
    /// What the value hashes to is, with the session, for
    /// `LINK_SECONDS`; a link that session was waiting for is forgotten.
    /// `None`: there is no such session.
    pub fn begin_link(&self, session: &str) -> Result<Option<String>, String> {
        let value = random_hex("a link")?;
        let link = Link { hash: hash_of(&value), until_ms: now_ms() + LINK_SECONDS * 1000 };
        let held = self.hold()?;
        let begun = self.write_sessions(&held, |sessions| {
            sessions.get_mut(&hash_of(session)).map(|of| of.link = Some(link)).is_some()
        })?;
        Ok(begun.then_some(value))
    }

    /// Whose link a value brought back is: the person, and what their
    /// session hashes to. The link is taken out as it is looked up: a
    /// value is used once, whatever follows. `None`: no session waits for
    /// it (it was used, it is too old, or its session ended).
    pub fn take_link(&self, value: &str) -> Result<Option<(String, String)>, String> {
        let wanted = hash_of(value);
        let waits = |session: &Session| session.link.as_ref().is_some_and(|link| link.hash == wanted);
        // Nothing is written for a value nobody waits for.
        if !self.sessions().sessions.values().any(waits) {
            return Ok(None);
        }
        let held = self.hold()?;
        self.write_sessions(&held, |sessions| {
            let (hash, session) = sessions.iter_mut().find(|(_, session)| waits(session))?;
            let link = session.link.take()?;
            (link.until_ms > now_ms()).then(|| (session.user.clone(), hash.clone()))
        })
    }

    /// What somebody is told when the identity they signed in with is
    /// nobody's: the same answer for whoever it is, nothing about who is
    /// known.
    pub fn nobody(identity: &Identity) -> String {
        let who = identity.written();
        format!(
            "{who} is nobody on this server: an administrator adds people (`kito-server user add <name> \
             --identity {who}`)"
        )
    }

    /// `true` if whoever signs in as this identity is this person.
    pub fn signs_in_as(&self, identity: &Identity, name: &str) -> bool {
        self.user_by_identity(identity).as_deref() == Some(name)
    }

    /// Ends a session, and tells the plugins that listen whose it was.
    /// Nothing happens for one that is not there. `Err`: it could not be
    /// ended, and goes on: whoever asked is told so.
    pub fn end_session(&self, token: &str) -> Result<(), String> {
        let hash = hash_of(token);
        if !self.sessions().sessions.contains_key(&hash) {
            return Ok(());
        }
        let held = self.hold()?;
        let ended = self.write_sessions(&held, |sessions| sessions.remove(&hash))?;
        // Kept and queued with the lock held: in the order it happened.
        if let Some(session) = ended {
            self.tell("session_ended", serde_json::json!({ "user": session.user, "what": session.user }));
        }
        drop(held);
        Ok(())
    }

    // ----------------------------------------------------------- plugins

    /// Enables a plugin on this server, with the permissions said or else
    /// with what it asks for; never with more than it asks for.
    pub fn enable_plugin(
        &mut self,
        name: &str,
        only: Option<Vec<kitod::plugins::Permission>>,
        for_clients: bool,
    ) -> Result<kitod::plugins::Installed, String> {
        use kitod::plugins;
        plugins::check_name("plugin", name)?;
        let found = plugins::installed(name).ok_or_else(|| {
            format!(
                "the plugin `{name}` is not installed for the user this server runs as: `kito-server plugin \
                 add --path <dir>` first"
            )
        })?;
        if let Some(why) = found.manifest.unfit().or_else(|| found.manifest.not_for("server")) {
            return Err(why);
        }
        let mut given = only.unwrap_or_else(|| found.manifest.permissions.clone());
        if let Some(more) = given.iter().find(|permission| !found.manifest.permissions.contains(permission)) {
            return Err(format!(
                "the plugin `{name}` does not ask for `{}`: it is given what it asks for, or less",
                more.word()
            ));
        }
        given.sort();
        given.dedup();
        let config = plugins::PluginConfig { enabled: true, permissions: given, for_clients };
        self.change(|change| -> Result<(), String> {
            change.config_mut().plugins.insert(name.to_owned(), config);
            Ok(())
        })?;
        Ok(found)
    }

    /// The plugins that run in the directories connected to this server
    /// too (ADR 0007), with what of a repository each was given.
    pub fn client_plugins(&self) -> Vec<(String, kitod::plugins::PluginConfig)> {
        let for_clients =
            self.config.plugins.iter().filter(|(_, config)| config.enabled && config.for_clients);
        for_clients
            .map(|(name, config)| {
                let permissions = config
                    .permissions
                    .iter()
                    .copied()
                    .filter(|permission| permission.of_a_repository())
                    .collect();
                (name.clone(), kitod::plugins::PluginConfig { enabled: true, permissions, for_clients: true })
            })
            .collect()
    }

    /// Asks a plugin of this server one thing for the same plugin on
    /// somebody's machine (ADR 0007): `kito-<plugin> answer <ask>`, with
    /// the parameters on its input and who asks in its environment. The
    /// answer is the last line it prints, and is data.
    pub fn answer(
        &self,
        plugin: &str,
        ask: &str,
        params: &serde_json::Value,
        (user, repo, role): (&str, &str, &str),
    ) -> Result<serde_json::Value, String> {
        let running = self.running();
        let found =
            running.iter().find(|(found, config)| found.manifest.name == plugin && config.for_clients);
        let Some((found, _)) = found else {
            return Err(format!(
                "this server has no plugin `{plugin}` that answers its clients (`kito-server plugin enable \
                 {plugin} --for-clients`)"
            ));
        };
        kitod::plugins::check_name("question", ask)?;
        let program = kitod::plugins::program(&found.dir, plugin);
        let envs = [
            (ENV_SERVER_DIR, self.dir.as_os_str().to_owned()),
            (ENV_ASKED_BY, user.into()),
            (ENV_ASKED_REPO, repo.into()),
            (ENV_ASKED_ROLE, role.into()),
        ];
        let input = serde_json::to_vec(params).unwrap_or_default();
        let within = std::time::Duration::from_secs(60);
        let printed =
            kitod::plugins::run_program(&program, &["answer", ask], &self.dir, &envs, &input, within)
                .map_err(|why| format!("the plugin `{plugin}` of the server failed at `{ask}`: {why}"))?;
        let text = String::from_utf8_lossy(&printed);
        let last = text.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or_default();
        serde_json::from_str(last).map_err(|_| {
            format!("the plugin `{plugin}` of the server answered `{ask}` with something that is no answer")
        })
    }

    /// Disables a plugin: nothing of it runs on this server. `false`: it
    /// was not enabled.
    pub fn disable_plugin(&mut self, name: &str, purge: bool) -> Result<bool, String> {
        let was = self.change(|change| -> Result<bool, String> {
            let enabled = change.config_mut().plugins.get_mut(name).filter(|plugin| plugin.enabled);
            Ok(enabled.map(|plugin| plugin.enabled = false).is_some())
        })?;
        let _ = std::fs::remove_file(self.failure_file(name));
        // What it kept of its own here stays for when it is enabled
        // again, unless asked otherwise.
        if purge && kitod::plugins::check_name("plugin", name).is_ok() {
            let _ = std::fs::remove_dir_all(self.plugin_dir(name));
        }
        Ok(was)
    }

    /// Forgets a plugin that is not enabled: the server says nothing of
    /// it any more. `Err`: it is enabled.
    pub fn forget_plugin(&mut self, name: &str) -> Result<(), String> {
        self.change(|change| {
            if change.config.plugins.get(name).is_some_and(|plugin| plugin.enabled) {
                return Err(format!(
                    "the plugin `{name}` is enabled on this server: `kito-server plugin disable {name}` first"
                ));
            }
            change.config_mut().plugins.remove(name);
            Ok(())
        })
    }

    /// The plugins that run here: enabled, installed, written for an API
    /// this Kito speaks; each with what it was given.
    pub fn running(&self) -> Vec<(kitod::plugins::Installed, &kitod::plugins::PluginConfig)> {
        let enabled = self.config.plugins.iter().filter(|(_, config)| config.enabled);
        enabled
            .filter_map(|(name, config)| Some((kitod::plugins::installed(name)?, config)))
            .filter(|(found, _)| found.manifest.unfit().is_none())
            .collect()
    }

    /// What a plugin keeps of its own on this server: `plugins/<name>/`.
    pub fn plugin_dir(&self, name: &str) -> PathBuf {
        self.dir.join("plugins").join(name)
    }

    fn failure_file(&self, name: &str) -> PathBuf {
        self.plugin_dir(name).join("last-failure")
    }

    /// What a plugin last failed at, if it has not run well since.
    pub fn last_failure(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.failure_file(name)).ok().map(|said| said.trim().to_owned())
    }

    /// Tells the plugins that listen what happened on the server: each is
    /// run for the kinds it listed, one at a time and in order, after the
    /// fact. One that fails is said, and held against nothing else.
    ///
    /// Kept in the audit log now, and told as everything is told
    /// (`queue`): in a process that serves, the events of every request
    /// take one way to the plugins, in the order the changes were made.
    pub fn tell(&self, kind: &str, event: serde_json::Value) {
        self.note(kind, &event);
        self.queue(vec![(kind.to_owned(), event)]);
    }

    /// Keeps what happened in the audit log: what changes the server is
    /// kept, whoever listens (ADR 0017).
    pub fn note(&self, kind: &str, event: &serde_json::Value) {
        let said = |key: &str| event.get(key).and_then(serde_json::Value::as_str).map(str::to_owned);
        let what = said("what").unwrap_or_else(|| event.to_string());
        // Who did it: whoever the event says; for a sign-in or a session, the person themselves.
        let by = match kind {
            "signed_in" | "session_ended" | "session_opened" => said("user"),
            _ => said("by"),
        };
        crate::audit::record(&self.dir, kind, by.as_deref(), said("repo").as_deref(), &what);
    }

    /// Keeps each of these in the audit log now, and tells the plugins
    /// that listen once whoever asked has their answer: on a thread of
    /// its own, one event at a time, in the order they are given. A
    /// plugin that is slow holds no answer back, and no token.
    pub fn tell_after(&self, events: Vec<(&'static str, serde_json::Value)>) {
        for (kind, event) in &events {
            self.note(kind, event);
        }
        self.queue(events.into_iter().map(|(kind, event)| (kind.to_owned(), event)).collect());
    }

    /// Gives the plugins events that are kept already (`note`), to be
    /// told on the thread that tells them: one at a time, in the order
    /// they are given, whoever gives them. `told` waits for them.
    pub fn queue(&self, events: Vec<(String, serde_json::Value)>) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NUMBERED: AtomicU64 = AtomicU64::new(0);
        // Numbered and sent under one lock: what waits is in the order it is told.
        let sender = later().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (kind, event) in events {
            let nth = NUMBERED.fetch_add(1, Ordering::SeqCst);
            #[cfg(test)]
            tests::queued().lock().unwrap().push((self.dir.clone(), kind.clone(), event.clone()));
            waiting().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(nth, kind.clone());
            if sender.send((self.dir.clone(), nth, kind, event)).is_err() {
                waiting().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&nth);
            }
        }
    }

    /// Runs the plugins that listen for what happened, without keeping it
    /// (`note` does): for whoever tells after they have answered.
    pub fn tell_plugins(&self, kind: &str, mut event: serde_json::Value) {
        use kitod::plugins::Permission;
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        event["kind"] = kind.into();
        event["at_ms"] = at_ms.into();
        let told = serde_json::to_vec(&event).unwrap_or_default();
        let envs = [(ENV_SERVER_DIR, self.dir.as_os_str().to_owned())];
        for (found, config) in self.running() {
            let name = &found.manifest.name;
            if !config.permissions.contains(&Permission::ServerEvents) {
                continue;
            }
            // Its own failure is not told back to it, over and over.
            if kind == "plugin_failed" && event["plugin"] == name.as_str() {
                continue;
            }
            for listens in
                found.manifest.server_events.iter().filter(|spec| spec.kinds.iter().any(|k| k == kind))
            {
                let program = kitod::plugins::program(&found.dir, name);
                let timeout = std::time::Duration::from_secs(listens.timeout_secs.unwrap_or(30).max(1));
                let ran = kitod::plugins::run_program(
                    &program,
                    &["server-event"],
                    &self.dir,
                    &envs,
                    &told,
                    timeout,
                );
                match ran {
                    Ok(_) => drop(std::fs::remove_file(self.failure_file(name))),
                    Err(why) => {
                        let what = format!("on `{kind}`: {why}");
                        say(format_args!("the plugin `{name}` failed {what}"));
                        let _ = std::fs::create_dir_all(self.plugin_dir(name));
                        let _ = std::fs::write(self.failure_file(name), &what);
                        let failed = serde_json::json!({ "plugin": name, "what": what });
                        // A failure to hear of a failure is kept, and not
                        // told on: two plugins that fail on it would tell
                        // each other for ever.
                        match kind {
                            "plugin_failed" => self.note("plugin_failed", &failed),
                            _ => self.tell("plugin_failed", failed),
                        }
                    }
                }
            }
        }
    }

    /// Whose session a session's token hashes to, until it ends.
    pub fn session_user(&self, hash: &str) -> Option<String> {
        let session = self.sessions().sessions.get(hash).cloned()?;
        (session.until_ms > now_ms()).then_some(session.user)
    }

    /// The person a token belongs to.
    pub fn user_of(&self, token: &str) -> Option<(String, User)> {
        let hash = hash_of(token);
        // A session: of somebody who signed in, until it ends.
        if token.starts_with(SESSION_PREFIX) {
            let name = self.session_user(&hash)?;
            let user = self.config.users.get(&name)?.clone();
            return Some((name, user));
        }
        if token.is_empty() {
            return None;
        }
        self.config
            .users
            .iter()
            .find(|(_, user)| user.token == hash)
            .map(|(name, user)| (name.clone(), user.clone()))
    }
}

/// What a change does to the configuration it has just read. Each checks
/// what it needs there and then: the person exists, the name is free,
/// they keep a way to sign in.
impl Change<'_> {
    /// The configuration, for what has no method here: it is written
    /// when the change is made.
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.server.config
    }

    /// Tells this once the change is written: kept in the audit log and
    /// queued for the plugins while its lock is still held, so that what
    /// changes after it is told after it. Nothing is told of a change that
    /// is not written.
    pub fn then_tell(&mut self, kind: &'static str, event: serde_json::Value) {
        self.told.push((kind, event.clone(), event));
    }

    /// Each of these, in order, as `then_tell`.
    pub fn then_tell_all(&mut self, events: Vec<(&'static str, serde_json::Value)>) {
        for (kind, event) in events {
            self.then_tell(kind, event);
        }
    }

    /// As `then_tell`, with what the audit log keeps apart from what the
    /// plugins are told.
    pub fn then_tell_kept(&mut self, kind: &'static str, kept: serde_json::Value, told: serde_json::Value) {
        self.told.push((kind, kept, told));
    }

    fn user(&self, name: &str) -> Result<&User, String> {
        self.server.config.users.get(name).ok_or_else(|| format!("{name} is not a user of this server"))
    }

    fn user_mut(&mut self, name: &str) -> Result<&mut User, String> {
        self.server.config.users.get_mut(name).ok_or_else(|| format!("{name} is not a user of this server"))
    }

    /// Adds a person and returns their token, which is not kept. With an
    /// identity they sign in instead, and have no token.
    pub fn add_user(
        &mut self,
        name: &str,
        email: Option<String>,
        access: &[(String, Role)],
        server_role: ServerRole,
        identity: Option<&Identity>,
    ) -> Result<Option<String>, String> {
        check_name("user", name)?;
        if self.config.users.contains_key(name) {
            return Err(format!(
                "{name} is a user already: `kito-server user token {name}` on the server's machine, or `kito \
                 people token {name}`, gives them a new token"
            ));
        }
        for (repo, _) in access {
            if self.repo(repo).is_none() {
                return Err(format!("there is no repository `{repo}` on this server"));
            }
        }
        let token = new_token()?;
        if let Some(taken) = identity.and_then(|identity| self.user_by_identity(identity)) {
            return Err(format!("{} is {taken} already", identity.map(Identity::key).unwrap_or_default()));
        }
        let user = User {
            email: email.or_else(|| identity.and_then(|identity| identity.email.clone())),
            // Nothing hashes to nothing: with an identity there is no token.
            token: identity.map_or_else(|| hash_of(&token), |_| String::new()),
            server_role,
            access: access.iter().cloned().collect(),
            identities: identity.iter().map(|identity| identity.key()).collect(),
            logins: identity.iter().map(|identity| identity.written()).collect(),
            ..User::default()
        };
        self.config_mut().users.insert(name.to_owned(), user);
        Ok(identity.is_none().then_some(token))
    }

    /// Changes what a person may do on the server itself.
    pub fn set_server_role(&mut self, name: &str, role: ServerRole) -> Result<(), String> {
        self.user_mut(name)?.server_role = role;
        Ok(())
    }

    /// Gives a person a repository, with a role in it; `None` takes it
    /// away. Answers whether anything changed.
    pub fn grant(&mut self, name: &str, repo: &str, role: Option<Role>) -> Result<bool, String> {
        if self.repo(repo).is_none() {
            return Err(format!("there is no repository `{repo}` on this server"));
        }
        let user = self.user_mut(name)?;
        let before = user.access.get(repo).copied();
        match role {
            Some(role) => drop(user.access.insert(repo.to_owned(), role)),
            None => drop(user.access.remove(repo)),
        }
        Ok(before != user.access.get(repo).copied())
    }

    /// Removes a person. Their sessions end with them, and before them:
    /// nobody is removed while their sessions could not be ended, for a
    /// later person of that name to find.
    pub fn remove_user(&mut self, name: &str) -> Result<(), String> {
        self.user(name)?;
        self.end_sessions(name, None)?;
        self.config_mut().users.remove(name);
        let _ = std::fs::remove_file(self.dir.join(SEEN_DIR).join(name));
        Ok(())
    }

    // ------------------------------- how a person signs in (ADR 0006, amended)
    //
    // A person has a token, identities, or both: never nothing. What would
    // leave them with no way to sign in is refused, and says what to do
    // first.
    //
    // What takes something away ends every session of the person but the
    // one it was asked through (`keep`, as what its token hashes to): a
    // session does not say what it was opened with, and one opened with
    // what is no longer theirs must not go on. The sessions end first,
    // and the configuration is written after, by the change: when the
    // sessions cannot be ended nothing changes, and no session is opened
    // in between, which takes the lock this holds.

    /// Attaches an identity to a person, who keeps what they signed in
    /// with. `false`: it was theirs already, and nothing changed. One
    /// identity is one person's, and a person has one of each way.
    pub fn link(&mut self, name: &str, identity: &Identity) -> Result<bool, String> {
        let written = identity.written();
        match self.user_by_identity(identity) {
            Some(theirs) if theirs == name => return Ok(false),
            Some(taken) => return Err(format!("{written} is {taken} already")),
            None => {}
        }
        let user = self.user_mut(name)?;
        if let Some(has) = user.login_of(&identity.provider) {
            return Err(format!(
                "{name} signs in with {has} already, and a person has one identity of each way: unlink it \
                 first"
            ));
        }
        user.identities.push(identity.key());
        user.logins.push(written);
        Ok(true)
    }

    /// Detaches a person's identity of a way to sign in, and ends their
    /// sessions but `keep`. Answers the login it was, and how many
    /// sessions ended.
    pub fn unlink(
        &mut self,
        name: &str,
        method: &str,
        keep: Option<&str>,
    ) -> Result<(String, usize), String> {
        let user = self.user(name)?;
        let Some(login) = user.login_of(method) else {
            return Err(format!("{name} has no identity of `{method}`: there is nothing to unlink"));
        };
        let left: Vec<String> =
            user.identities.iter().filter(|kept| !of_method(kept, method)).cloned().collect();
        self.keeps_a_way(name, user.has_token(), &left, "give them a token first")?;
        let ended = self.end_sessions(name, keep)?;
        let user = self.user_mut(name)?;
        user.identities.retain(|kept| !of_method(kept, method));
        user.logins.retain(|kept| !of_method(kept, method));
        Ok((login, ended))
    }

    /// Gives a person a new token and returns it, which is not kept. The
    /// one they had, if they had one (`true`), is theirs no longer, and
    /// their sessions but `keep` end: how many is answered. A first token
    /// ends nothing: there was none to open anything with.
    pub fn change_token(&mut self, name: &str, keep: Option<&str>) -> Result<(String, bool, usize), String> {
        let token = new_token()?;
        let replaced = self.user(name)?.has_token();
        let ended = match replaced {
            true => self.end_sessions(name, keep)?,
            false => 0,
        };
        self.user_mut(name)?.token = hash_of(&token);
        Ok((token, replaced, ended))
    }

    /// Takes a person's token away: they sign in from then on, and their
    /// sessions but `keep` end. Answers how many.
    pub fn remove_token(&mut self, name: &str, keep: Option<&str>) -> Result<usize, String> {
        let user = self.user(name)?;
        if !user.has_token() {
            return Err(format!("{name} has no token: there is nothing to take away"));
        }
        self.keeps_a_way(name, false, &user.identities, "link an identity to them first")?;
        let ended = self.end_sessions(name, keep)?;
        self.user_mut(name)?.token.clear();
        Ok(ended)
    }

    /// For a token that leaked (`--alone`): a new token, which is returned
    /// and not kept, and nothing else to sign in with. Every identity of
    /// the person is unlinked, whoever attached it, and every session but
    /// `keep` ends, in this one change. Answers the logins taken away,
    /// whether they had a token, and how many sessions ended.
    pub fn token_alone(
        &mut self,
        name: &str,
        keep: Option<&str>,
    ) -> Result<(String, Vec<String>, bool, usize), String> {
        let token = new_token()?;
        let user = self.user(name)?;
        let ways = user.identities.iter().filter_map(|kept| kept.split_once(':').map(|(way, _)| way));
        let unlinked: Vec<String> = ways.filter_map(|way| user.login_of(way)).collect();
        let replaced = user.has_token();
        let ended = self.end_sessions(name, keep)?;
        let user = self.user_mut(name)?;
        user.token = hash_of(&token);
        user.identities.clear();
        user.logins.clear();
        Ok((token, unlinked, replaced, ended))
    }

    /// Whether a person would still sign in, with a token (`token`) or
    /// with one of these identities; `Err`: they would not, why, and what
    /// to do `first`. An identity counts while a plugin of the server
    /// serves its way: one of a way that is offered no more signs nobody
    /// in, and is not what is left to somebody.
    fn keeps_a_way(&self, name: &str, token: bool, identities: &[String], first: &str) -> Result<(), String> {
        let offered = self.sign_in_methods();
        let signs_in = |kept: &String| offered.iter().any(|(way, _)| of_method(kept, way));
        if token || identities.iter().any(signs_in) {
            return Ok(());
        }
        let user = self.user(name)?;
        let logins: Vec<String> = identities
            .iter()
            .filter_map(|kept| kept.split_once(':'))
            .filter_map(|(way, _)| user.login_of(way))
            .collect();
        Err(match logins.is_empty() {
            true => format!("{name} would have no way to sign in: {first}"),
            false => format!(
                "{name} would have no way to sign in: {} is all they would have, and this server offers \
                 that way no more (no plugin enabled here signs people in with it): {first}",
                logins.join(", ")
            ),
        })
    }

    /// Ends every session of a person but the one whose token hashes to
    /// `keep`, and answers how many ended. The plugins are told after the
    /// change, by whoever made it.
    ///
    /// What it ended is noted on the change: they stay ended if the
    /// change is not written, and that is kept and told then (`change`).
    fn end_sessions(&mut self, name: &str, keep: Option<&str>) -> Result<usize, String> {
        let ended = self.server.write_sessions(self.held, |sessions| {
            let before = sessions.len();
            sessions.retain(|hash, session| session.user != name || Some(hash.as_str()) == keep);
            before - sessions.len()
        })?;
        self.ended.push((name.to_owned(), ended));
        Ok(ended)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A file in a server's directory that keeps its sessions from being
    /// written, in these tests: what a full disk would do.
    pub(super) const UNWRITABLE: &str = "sessions-unwritable-in-the-tests";

    /// Whether a step is made to fail in these tests, in this server's
    /// directory alone: `fail-<step>-in-the-tests` is there.
    /// Takes its time between a change that adds somebody and what it
    /// tells, in these tests, in a server's directory with
    /// `pause-telling-in-the-tests`: what is made after it is quicker.
    pub(super) fn pausing(dir: &Path, told: &[(&'static str, serde_json::Value, serde_json::Value)]) {
        let adds = told.iter().any(|(kind, _, _)| *kind == "user_added");
        if adds && dir.join("pause-telling-in-the-tests").exists() {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }

    pub(super) fn failing(dir: &Path, step: &str) -> Result<(), String> {
        match dir.join(format!("fail-{step}-in-the-tests")).exists() {
            true => Err(format!("{step} fails in the tests")),
            false => Ok(()),
        }
    }

    /// A person added with a token: the token.
    fn person(server: &mut Server, name: &str) -> String {
        server.add_user(name, None, &[], ServerRole::Member, None).unwrap().expect("a token")
    }

    /// Whether a token is bob's, of the server as it is.
    fn bobs(token: &str) -> impl FnOnce(&Server) -> bool {
        let token = token.to_owned();
        move |now: &Server| now.user_of(&token).is_some_and(|(name, _)| name == "bob")
    }

    /// How many sessions a person has.
    fn sessions_of(server: &Server, name: &str) -> usize {
        server.sessions().sessions.values().filter(|session| session.user == name).count()
    }

    #[test]
    fn no_session_opens_with_a_token_changed_since_it_was_looked_at() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let token = person(&mut server, "bob");
        let session =
            server.open_session("bob", ("a token", None), bobs(&token)).unwrap().expect("his token is his");
        assert_eq!(server.user_of(&session).map(|(name, _)| name).as_deref(), Some("bob"));

        // Somebody looked at his token while it was his, and writes the
        // session once it is changed: the one it had opened ended with
        // the change, and this one does not open.
        let looked = Server::open(dir.path()).unwrap();
        assert!(looked.user_of(&token).is_some());
        let (new, replaced, ended) = server.change_token("bob", None).unwrap();
        assert!(replaced && new != token && ended == 1);
        assert!(server.user_of(&session).is_none());
        assert_eq!(looked.open_session("bob", ("a token", None), bobs(&token)), Ok(None));
        assert_eq!(sessions_of(&server, "bob"), 0, "nothing was written");

        // The new one opens sessions; the one a change is asked through stays.
        let kept = server.open_session("bob", ("a token", None), bobs(&new)).unwrap().unwrap();
        let other = server.open_session("bob", ("a token", None), bobs(&new)).unwrap().unwrap();
        let (_, _, ended) = server.change_token("bob", Some(&hash_of(&kept))).unwrap();
        assert_eq!((ended, sessions_of(&server, "bob")), (1, 1));
        assert!(server.user_of(&kept).is_some() && server.user_of(&other).is_none());
    }

    /// What is staged in a server's directory, and should not be left there.
    fn staged(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
            .filter(|name| name.ends_with(".new"))
            .collect()
    }

    /// Makes a file as old as one nobody is writing any more.
    fn aged(path: &Path) {
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(then).unwrap();
    }

    #[test]
    fn people_who_change_their_tokens_at_once_each_end_with_the_token_they_were_answered() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let people = ["ada", "bob", "carol", "dave"];
        let first: Vec<String> = people.iter().map(|name| person(&mut server, name)).collect();
        let writing: Vec<_> = people
            .into_iter()
            .map(|name| {
                let dir = dir.path().to_owned();
                std::thread::spawn(move || {
                    let mut answered = Vec::new();
                    for _ in 0..25 {
                        // As a request does: the server as it was read, changed, written.
                        let mut now = Server::open(&dir).expect("the configuration is whole");
                        let (token, replaced, _) = now.change_token(name, None).expect("it is written");
                        assert!(replaced, "{name} had no token: somebody wrote over it");
                        answered.push(token);
                    }
                    answered
                })
            })
            .collect();
        let answered: Vec<Vec<String>> = writing.into_iter().map(|thread| thread.join().unwrap()).collect();
        let after = Server::open(dir.path()).expect("the configuration is whole");
        assert_eq!(after.config.users.len(), people.len());
        for ((name, first), answered) in people.iter().zip(&first).zip(&answered) {
            // The last each was answered is theirs, and nothing they replaced is anybody's.
            let (last, replaced) = answered.split_last().unwrap();
            assert_eq!(after.user_of(last).map(|(whose, _)| whose).as_deref(), Some(*name));
            for old in replaced.iter().chain([first]) {
                assert!(after.user_of(old).is_none(), "a token {name} replaced works again");
            }
        }
        // Nothing staged is left behind.
        assert_eq!(staged(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_change_that_began_before_a_token_was_replaced_does_not_bring_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        person(&mut server, "ada");
        person(&mut server, "carol");
        let leaked = person(&mut server, "bob");
        // ada's request read the server, and takes its time: a clone.
        let mut slow = Server::open(dir.path()).unwrap();
        assert!(slow.user_of(&leaked).is_some());
        // Meanwhile bob's token is replaced, bob is made an admin and carol is removed.
        let mut owner = Server::open(dir.path()).unwrap();
        let (new, _, _) = owner.change_token("bob", None).unwrap();
        owner.set_server_role("bob", ServerRole::Admin).unwrap();
        owner.remove_user("carol").unwrap();
        // What ada's request changes at its end, it changes on the server as it is by then.
        slow.set_server_role("ada", ServerRole::Admin).unwrap();
        for server in [&slow, &Server::open(dir.path()).unwrap()] {
            assert!(server.user_of(&leaked).is_none(), "the token that was replaced works again");
            assert_eq!(server.user_of(&new).map(|(name, _)| name).as_deref(), Some("bob"));
            assert_eq!(server.config.users["bob"].server_role, ServerRole::Admin);
            assert_eq!(server.config.users["ada"].server_role, ServerRole::Admin);
            assert!(!server.config.users.contains_key("carol"), "carol is back");
        }
        // A change that is refused leaves it on what is on the disk, too.
        owner.set_server_role("bob", ServerRole::Member).unwrap();
        assert!(slow.grant("bob", "nowhere", Some(Role::Read)).is_err());
        assert_eq!(slow.config.users["bob"].server_role, ServerRole::Member);
    }

    #[test]
    fn a_writer_that_did_not_read_again_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let token = person(&mut server, "bob");
        let mut stale = Server::open(dir.path()).unwrap();
        let (new, _, _) = server.change_token("bob", None).unwrap();
        // Whatever would write what it read a while ago, as every writer once did.
        stale.config.users.get_mut("bob").unwrap().server_role = ServerRole::Owner;
        let held = stale.hold().unwrap();
        assert_eq!(stale.save(&held), Err(CHANGED_MEANWHILE.to_owned()));
        drop(held);
        let after = Server::open(dir.path()).unwrap();
        assert_eq!(after.user_of(&new).map(|(name, _)| name).as_deref(), Some("bob"));
        assert!(after.user_of(&token).is_none(), "the token that was replaced works again");
        assert_eq!(after.config.users["bob"].server_role, ServerRole::Member);
        assert_eq!(staged(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_change_is_made_only_while_whoever_asked_may_still_ask() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        person(&mut server, "ada");
        person(&mut server, "bob");
        server.set_server_role("ada", ServerRole::Admin).unwrap();
        // A request of ada's, an admin when it began.
        let mut hers = Server::open(dir.path()).unwrap();
        hers.only_while(|now: &Server| match now.config.users.get("ada").is_some_and(User::is_admin) {
            true => Ok(()),
            false => Err("ada is no longer an admin of this server".to_owned()),
        });
        hers.set_server_role("bob", ServerRole::Admin).unwrap();
        // She is made a member while it takes its time: what it changes then, it does not.
        server.set_server_role("ada", ServerRole::Member).unwrap();
        let refused = hers.remove_user("bob").unwrap_err();
        assert_eq!(refused, "ada is no longer an admin of this server");
        assert!(
            hers.forget_plugin("hello").is_err()
                && hers.delete_repo("shop", Some(Which::Live), None).is_err()
        );
        assert!(Server::open(dir.path()).unwrap().config.users.contains_key("bob"));
    }

    #[test]
    fn when_the_sessions_cannot_be_ended_nothing_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let token = person(&mut server, "bob");
        let hello = Identity {
            provider: "hello".to_owned(),
            subject: "id-bob".to_owned(),
            login: "bob".to_owned(),
            name: None,
            email: None,
        };
        assert_eq!(server.link("bob", &hello), Ok(true));
        let session = server.open_session("bob", ("a token", None), bobs(&token)).unwrap().unwrap();
        // The sessions cannot be written.
        let in_the_way = dir.path().join(UNWRITABLE);
        std::fs::write(&in_the_way, "").unwrap();
        let config = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(server.change_token("bob", None).is_err(), "a new token, and the sessions go on");
        assert!(server.unlink("bob", "hello", None).is_err(), "unlinked, and the sessions go on");
        assert!(server.remove_user("bob").is_err(), "removed, and the sessions are left behind");
        assert_eq!(std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(), config);
        // The token he had is his still, and so is his session: nothing is half done.
        for server in [&server, &Server::open(dir.path()).unwrap()] {
            assert_eq!(server.user_of(&token).map(|(name, _)| name).as_deref(), Some("bob"));
            assert_eq!(server.user_of(&session).map(|(name, _)| name).as_deref(), Some("bob"));
            assert_eq!(server.config.users["bob"].logins, ["hello:bob"]);
        }
        // Once they can be ended, they are, and then the token changes.
        std::fs::remove_file(&in_the_way).unwrap();
        let (new, replaced, ended) = server.change_token("bob", None).unwrap();
        assert!(replaced && ended == 1 && server.user_of(&session).is_none());
        assert!(server.user_of(&token).is_none() && server.user_of(&new).is_some());
    }

    #[test]
    fn a_lock_that_cannot_be_taken_is_said_and_nothing_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let token = person(&mut server, "bob");
        let session = server.open_session("bob", ("a token", None), bobs(&token)).unwrap().unwrap();
        let held_by_another = "held by something that does not finish: nothing was changed";
        // Somebody holds what the server keeps, and goes on holding it.
        let held = Lock::take(dir.path()).expect("nobody holds it");
        let refused = server.end_session(&session).unwrap_err();
        assert!(refused.contains(held_by_another), "{refused}");
        assert!(server.user_of(&session).is_some(), "it was told it ended, and goes on");
        drop(held);
        // A person is not removed while their sessions could not be ended.
        let held = Lock::take(dir.path()).expect("nobody holds it");
        let refused = server.remove_user("bob").unwrap_err();
        assert!(refused.contains(held_by_another), "{refused}");
        drop(held);
        let after = Server::open(dir.path()).unwrap();
        assert!(after.user_of(&token).is_some() && after.user_of(&session).is_some());
        // Let go, both are done: and his sessions go with him.
        server.remove_user("bob").unwrap();
        assert_eq!(sessions_of(&server, "bob"), 0);
        assert_eq!(server.end_session(&session), Ok(()));
    }

    /// What a process run by the tests below says once it holds the lock.
    const HOLDING: &str = "kito-test: the lock is held by another process";

    /// Run by the tests below as a process of its own: holds the lock of
    /// the directory `KITO_TEST_HOLD` names, says so, and holds it until
    /// its input closes or it is killed. Run with the others, it does
    /// nothing.
    #[test]
    fn holds_a_lock_for_another_process() {
        use std::io::{Read as _, Write as _};
        let Some(dir) = std::env::var_os("KITO_TEST_HOLD") else { return };
        let _held = Lock::take(Path::new(&dir)).expect("nobody holds it");
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{HOLDING}");
        let _ = out.flush();
        let _ = std::io::stdin().read(&mut [0u8]);
    }

    /// Run by the tests below as a process of its own: gives the person
    /// `KITO_TEST_TOKENS` names (`<dir>|<name>`) a new token 20 times, as
    /// requests do, and prints each.
    #[test]
    fn changes_tokens_for_another_process() {
        let Some(asked) = std::env::var("KITO_TEST_TOKENS").ok() else { return };
        let (dir, name) = asked.split_once('|').unwrap();
        for _ in 0..20 {
            let mut now = Server::open(Path::new(dir)).expect("the configuration is whole");
            let (token, replaced, _) = now.change_token(name, None).expect("it is written");
            assert!(replaced, "{name} had no token: somebody wrote over it");
            println!("{token}");
        }
    }

    /// This test program, run for one of the tests above with `env` set.
    fn another_process(test: &str, env: (&str, &std::ffi::OsStr)) -> std::process::Child {
        use std::process::Stdio;
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("state::tests::{test}"), "--nocapture", "--test-threads", "1"])
            .env(env.0, env.1)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Another process that holds the lock of `dir`, once it says it does.
    fn holder(dir: &Path) -> std::process::Child {
        use std::io::BufRead as _;
        let mut child =
            another_process("holds_a_lock_for_another_process", ("KITO_TEST_HOLD", dir.as_os_str()));
        let mut said = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains(HOLDING) {
            line.clear();
            assert!(said.read_line(&mut line).unwrap() > 0, "the other process holds nothing");
        }
        // What it says after that is read, for it not to stop on it.
        std::thread::spawn(move || std::io::copy(&mut said, &mut std::io::sink()));
        child
    }

    #[test]
    fn a_lock_is_never_taken_from_a_process_that_holds_it_and_goes_with_one_that_dies() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        person(&mut server, "bob");
        let role = || Server::open(dir.path()).unwrap().config.users["bob"].server_role;
        // Another process holds it for longer than anybody writes: it is
        // its still, and whoever asks meanwhile is told so.
        let mut slow = holder(dir.path());
        aged(&dir.path().join(".lock"));
        let refused = server.set_server_role("bob", ServerRole::Owner).unwrap_err();
        assert!(refused.contains("held by something that does not finish: nothing was changed"), "{refused}");
        assert_eq!(role(), ServerRole::Member);
        // Let go, it is had at once.
        drop(slow.stdin.take());
        slow.wait().unwrap();
        server.set_server_role("bob", ServerRole::Admin).unwrap();
        assert_eq!(role(), ServerRole::Admin);

        // One that is killed while it holds it holds nothing more: nobody
        // waits for it.
        let mut killed = holder(dir.path());
        killed.kill().unwrap();
        killed.wait().unwrap();
        let asked = std::time::Instant::now();
        server.set_server_role("bob", ServerRole::Owner).unwrap();
        assert!(asked.elapsed() < std::time::Duration::from_secs(2), "{:?}", asked.elapsed());
        assert_eq!(role(), ServerRole::Owner);
        assert!(dir.path().join(".lock").is_file(), "the lock is a file that stays");

        // The audit log's lock is held the same way: a line is not
        // written past another process that holds it.
        crate::audit::record(dir.path(), "role_changed", None, None, "bob: owner");
        let mut slow = holder(&dir.path().join("audit"));
        aged(&dir.path().join("audit/.lock"));
        crate::audit::record(dir.path(), "role_changed", None, None, "bob: admin");
        assert_eq!(crate::audit::verify(dir.path()), Ok(1), "a line was written past the lock");
        drop(slow.stdin.take());
        slow.wait().unwrap();
        crate::audit::record(dir.path(), "role_changed", None, None, "bob: admin");
        assert_eq!(crate::audit::verify(dir.path()), Ok(2));
    }

    #[test]
    fn processes_that_change_tokens_at_once_each_end_with_the_token_they_were_answered() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let people = ["ada", "bob"];
        let first: Vec<String> = people.iter().map(|name| person(&mut server, name)).collect();
        let writing: Vec<_> = people
            .iter()
            .map(|name| {
                let asked = format!("{}|{name}", dir.path().display());
                another_process("changes_tokens_for_another_process", ("KITO_TEST_TOKENS", asked.as_ref()))
            })
            .collect();
        let answered: Vec<Vec<String>> = writing
            .into_iter()
            .map(|child| {
                let out = child.wait_with_output().unwrap();
                assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
                // What the test harness prints is around them, on their lines too.
                let printed = String::from_utf8_lossy(&out.stdout).into_owned();
                let words = printed.split_whitespace();
                words
                    .filter(|word| word.starts_with("kito_") && word.len() == 5 + 64)
                    .map(str::to_owned)
                    .collect()
            })
            .collect();
        let after = Server::open(dir.path()).unwrap();
        for ((name, first), answered) in people.iter().zip(&first).zip(&answered) {
            assert_eq!(answered.len(), 20, "{name}: {answered:?}");
            let (last, replaced) = answered.split_last().unwrap();
            assert_eq!(after.user_of(last).map(|(whose, _)| whose).as_deref(), Some(*name));
            for old in replaced.iter().chain([first]) {
                assert!(after.user_of(old).is_none(), "a token {name} replaced works again");
            }
        }
        assert_eq!(staged(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn sessions_a_change_ended_are_kept_and_said_when_the_change_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let token = person(&mut server, "bob");
        for _ in 0..2 {
            server.open_session("bob", ("a token", None), bobs(&token)).unwrap().unwrap();
        }
        // `server.toml` is edited by hand while the change is made: what
        // the change wrote of it is not written.
        let config = dir.path().join(CONFIG_FILE);
        let refused = server
            .change(|change| {
                let changed = change.change_token("bob", None);
                let text = std::fs::read_to_string(&config).unwrap();
                std::fs::write(&config, format!("{text}\n# edited by hand\n")).unwrap();
                changed
            })
            .unwrap_err();
        assert_eq!(refused, format!("{CHANGED_MEANWHILE}; 2 sessions of bob had already ended"));
        // The token is his still; his sessions ended, and that is kept.
        assert!(Server::open(dir.path()).unwrap().user_of(&token).is_some());
        assert_eq!(sessions_of(&server, "bob"), 0);
        let ended =
            crate::audit::Filter { kind: Some("session_ended".to_owned()), limit: 10, ..Default::default() };
        let kept = crate::audit::read(dir.path(), &ended);
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(kept.iter().all(|line| line["by"] == "bob"), "{kept:?}");
        told();
    }

    /// The lanes open in a repository, each as its id and why it was opened.
    fn lanes_of(root: &Path) -> Vec<String> {
        let repo = kitod::Repo::open(root).unwrap();
        let lanes = repo.lanes(false).into_iter();
        let mut lanes: Vec<String> =
            lanes.map(|lane| format!("{} {}", lane.id, lane.intent.unwrap_or_default())).collect();
        repo.close().unwrap();
        lanes.sort();
        lanes
    }

    #[test]
    fn the_purge_of_an_archived_repository_deletes_it_and_never_one_in_use_of_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        person(&mut server, "ada");
        person(&mut server, "bob");
        server.add_repo("shop", None, None, false).unwrap();
        server.grant("ada", "shop", Some(Role::Maintain)).unwrap();
        server.archive_repo("shop", Some("ada")).unwrap();
        // Nobody adds another `shop` any more: the name is the archived one's.
        let refused = server.add_repo("shop", None, None, false).unwrap_err();
        assert!(refused.contains("an archived repository is called `shop`"), "{refused}");
        // A server from before could: another `shop` is here as such a
        // server left it, with lanes of its own, and bob uses it.
        let given = FirstFiles::new(&[("README.md", "# the new shop\n")]).unwrap();
        let made = server.add_repo("shop-again", None, Some(&given), false).unwrap();
        let root = dir.path().join(REPOS_DIR).join("shop");
        std::fs::rename(&made, &root).unwrap();
        let mut repo = kitod::Repo::open(&root).unwrap();
        for intent in ["the first change", "the second change"] {
            repo.open_lane("bob", None, None, intent, &["README.md".to_owned()]).unwrap();
        }
        repo.close().unwrap();
        server.grant("bob", "shop", Some(Role::Write)).unwrap();
        let lanes = lanes_of(&root);
        assert_eq!(lanes.len(), 2, "{lanes:?}");
        assert!(lanes[0].ends_with(" the first change") && lanes[1].ends_with(" the second change"));
        let config = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();

        // Its days over, the archived one is purged: it, and nothing else.
        let said = dir.path().join("archived/shop.json");
        std::fs::write(&said, r#"{"at_ms": 0, "by": "ada"}"#).unwrap();
        assert_eq!(server.purge_archived(), ["shop"]);
        assert!(!dir.path().join("archived/shop").exists() && !said.exists());
        assert_eq!(server.repo("shop").as_deref(), Some(root.as_path()), "the shop in use is gone");
        assert_eq!(std::fs::read_to_string(root.join("README.md")).unwrap(), "# the new shop\n");
        assert_eq!(lanes_of(&root), lanes);
        // Who may use it is as it was.
        assert_eq!(std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(), config);
        let after = Server::open(dir.path()).unwrap();
        assert_eq!(after.config.users["bob"].access.get("shop"), Some(&Role::Write));
        // Nothing else is purged: there is nothing archived any more.
        assert_eq!(server.purge_archived(), Vec::<String>::new());
        assert!(server.repo("shop").is_some());
        let left: Vec<String> = std::fs::read_dir(dir.path().join(ARCHIVED_DIR))
            .unwrap()
            .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
            .filter(|name| name.starts_with(".deleted-"))
            .collect();
        assert_eq!(left, Vec::<String>::new());
    }

    #[test]
    fn an_owner_who_deletes_a_repository_says_which_of_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        server.add_repo("shop", None, None, false).unwrap();
        server.archive_repo("shop", Some("ada")).unwrap();
        assert_eq!(server.which_repo("shop", None), Ok(Which::Archived));
        assert!(server.which_repo("shop", Some(Which::Live)).unwrap_err().contains("no repository `shop`"));
        // Two of one name, as a server from before 0.39.5 may have them.
        let made = server.add_repo("shop-again", None, None, false).unwrap();
        let root = dir.path().join(REPOS_DIR).join("shop");
        std::fs::rename(&made, &root).unwrap();
        // Both are there: neither is guessed at.
        let both = server.which_repo("shop", None).unwrap_err();
        assert!(both.contains("and an archived one of that name: say which"), "{both}");
        assert_eq!(server.delete_repo("shop", None, None), Err(both));
        assert!(server.repo("shop").is_some() && dir.path().join("archived/shop").is_dir());
        assert_eq!(server.which_repo("shop", Some(Which::Archived)), Ok(Which::Archived));
        assert_eq!(server.which_repo("shop", Some(Which::Live)), Ok(Which::Live));
        // The archived one, deleted now: the one in use stays.
        assert_eq!(server.delete_repo("shop", Some(Which::Archived), None), Ok(Which::Archived));
        assert!(server.repo("shop").is_some() && root.join("README.md").is_file());
        let gone = server.delete_repo("shop", Some(Which::Archived), None).unwrap_err();
        assert_eq!(gone, "no archived repository is called `shop`");
        assert_eq!(server.which_repo("shop", None), Ok(Which::Live));
        server.delete_repo("shop", Some(Which::Live), None).unwrap();
        assert!(server.repo("shop").is_none() && !root.exists());
    }

    #[test]
    fn a_repository_is_archived_and_brought_back_with_the_lock_held() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::init(dir.path()).unwrap();
        server.add_repo("shop", None, None, false).unwrap();
        let held_by_another = "held by something that does not finish: nothing was changed";
        let held = Lock::take(dir.path()).unwrap();
        let refused = server.archive_repo("shop", Some("ada")).unwrap_err();
        assert!(refused.contains(held_by_another), "{refused}");
        assert!(server.repo("shop").is_some());
        drop(held);
        server.archive_repo("shop", Some("ada")).unwrap();
        let held = Lock::take(dir.path()).unwrap();
        let refused = server.unarchive_repo("shop", None).unwrap_err();
        assert!(refused.contains(held_by_another), "{refused}");
        assert!(server.repo("shop").is_none());
        drop(held);
        server.unarchive_repo("shop", None).unwrap();
        assert!(server.repo("shop").is_some());
    }

    #[test]
    fn changes_of_settings_noted_at_once_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let server = std::sync::Arc::new(Server::init(dir.path()).unwrap());
        let noting: Vec<_> = (0..6)
            .map(|n| {
                let server = server.clone();
                std::thread::spawn(move || {
                    for k in 0..20 {
                        server.note_setting("shop", &format!("w{n}"), "approvals", &serde_json::json!(k));
                    }
                })
            })
            .collect();
        for thread in noting {
            thread.join().unwrap();
        }
        assert_eq!(server.settings_history("shop").len(), 120);
        // When people were last here is written the same way.
        server.saw("bob");
        assert!(server.seen("bob").is_some());
        // Nothing staged is left behind.
        for kept in [SETTINGS_DIR, SEEN_DIR] {
            let left: Vec<String> = std::fs::read_dir(dir.path().join(kept))
                .unwrap()
                .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
                .filter(|name| name.ends_with(".new"))
                .collect();
            assert_eq!(left, Vec::<String>::new(), "{kept}");
        }
    }

    #[test]
    fn a_token_given_alone_leaves_nothing_else_to_sign_in_with() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let leaked = person(&mut server, "bob");
        // Whoever held his token attached an account of their own to him.
        let theirs = Identity {
            provider: "hello".to_owned(),
            subject: "id-mallory".to_owned(),
            login: "mallory".to_owned(),
            name: None,
            email: None,
        };
        assert_eq!(server.link("bob", &theirs), Ok(true));
        let kept = server.open_session("bob", ("a token", None), bobs(&leaked)).unwrap().unwrap();
        let other = server.open_session("bob", ("hello:mallory", None), |_| true).unwrap().unwrap();
        let (new, unlinked, replaced, ended) = server.token_alone("bob", Some(&hash_of(&kept))).unwrap();
        assert_eq!((unlinked.as_slice(), replaced, ended), (&["hello:mallory".to_owned()][..], true, 1));
        for now in [&server, &Server::open(dir.path()).unwrap()] {
            assert!(now.user_of(&leaked).is_none() && now.user_of(&other).is_none());
            assert_eq!(now.user_of(&new).map(|(name, _)| name).as_deref(), Some("bob"));
            assert_eq!(now.user_of(&kept).map(|(name, _)| name).as_deref(), Some("bob"));
            assert_eq!(now.user_by_identity(&theirs), None, "whoever signs in as mallory is bob still");
            assert!(
                now.config.users["bob"].identities.is_empty() && now.config.users["bob"].logins.is_empty()
            );
        }
        // What is kept and told names what was taken away.
        let events = Server::alone_events("bob", Some("bob"), (&unlinked, replaced), ended);
        let kinds: Vec<&str> = events.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(kinds, ["token_changed", "identity_unlinked", "session_ended"]);
        assert_eq!(events[0].1["what"], "bob, alone: hello:mallory unlinked");
        assert_eq!(events[0].1["unlinked"], serde_json::json!(["hello:mallory"]));
    }

    #[test]
    fn a_person_has_so_many_sessions_and_one_more_ends_their_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        person(&mut server, "bob");
        person(&mut server, "carol");
        let later = || std::thread::sleep(std::time::Duration::from_millis(3));
        let hers = server.open_session("carol", ("a token", None), |_| true).unwrap().unwrap();
        let oldest = server.open_session("bob", ("a token", None), |_| true).unwrap().unwrap();
        later();
        let rest: Vec<String> = (1..SESSIONS_EACH)
            .map(|_| server.open_session("bob", ("a token", None), |_| true).unwrap().unwrap())
            .collect();
        assert_eq!(sessions_of(&server, "bob"), SESSIONS_EACH);
        assert!(server.user_of(&oldest).is_some());
        later();
        let one_more = server.open_session("bob", ("a token", None), |_| true).unwrap().unwrap();
        assert_eq!(sessions_of(&server, "bob"), SESSIONS_EACH);
        assert!(server.user_of(&oldest).is_none(), "his oldest goes on");
        assert!(server.user_of(&one_more).is_some());
        assert!(rest.iter().all(|kept| server.user_of(kept).is_some()), "a later one ended");
        assert!(server.user_of(&hers).is_some(), "somebody else's ended");
        // That it ended is kept, as for one that is closed.
        let ended =
            crate::audit::Filter { kind: Some("session_ended".to_owned()), limit: 10, ..Default::default() };
        let kept = crate::audit::read(dir.path(), &ended);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0]["by"], "bob");
        // Nobody listens here; what tells them is done before the directory goes.
        told();
    }

    #[test]
    fn a_link_is_found_by_its_value_once_and_by_nothing_that_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let token = person(&mut server, "bob");
        let session = server.open_session("bob", ("a token", None), bobs(&token)).unwrap().unwrap();
        let kept = || std::fs::read_to_string(dir.path().join(SESSIONS_FILE)).unwrap();
        let his = Some(("bob".to_owned(), hash_of(&session)));

        let value = server.begin_link(&session).unwrap().expect("his session is there");
        assert_eq!(value.len(), 64);
        assert!(!kept().contains(&value) && kept().contains(&hash_of(&value)), "{}", kept());
        // What is on the disk brings nobody back: not what the link
        // hashes to, and not what the session does.
        assert_eq!(server.take_link(&hash_of(&value)), Ok(None));
        assert_eq!(server.take_link(&hash_of(&session)), Ok(None));
        assert_eq!(server.take_link(&session), Ok(None));
        // The value does, once.
        assert_eq!(server.take_link(&value), Ok(his.clone()));
        assert_eq!(server.take_link(&value), Ok(None));
        assert!(!kept().contains("link"), "{}", kept());

        // A link asked again is the one that waits; one that is too old is taken out, and brings nobody.
        let (before, again) = (server.begin_link(&session).unwrap().unwrap(), server.begin_link(&session));
        let again = again.unwrap().unwrap();
        assert_eq!(server.take_link(&before), Ok(None));
        assert_eq!(server.take_link(&again), Ok(his));
        let late = server.begin_link(&session).unwrap().unwrap();
        let held = server.hold().unwrap();
        let waits = |sessions: &mut BTreeMap<String, Session>| {
            let link = sessions.values_mut().find_map(|session| session.link.as_mut());
            link.expect("it waits").until_ms = now_ms() - 1;
        };
        server.write_sessions(&held, waits).unwrap();
        drop(held);
        assert_eq!(server.take_link(&late), Ok(None));
        assert!(!kept().contains("link"), "{}", kept());

        // It ends with its session, and none begins without one.
        let last = server.begin_link(&session).unwrap().unwrap();
        server.end_session(&session).unwrap();
        assert_eq!(server.take_link(&last), Ok(None));
        assert_eq!(server.begin_link(&session), Ok(None));
        assert_eq!(server.begin_link("kitos_nobody"), Ok(None));
    }

    #[test]
    fn what_a_stopped_server_left_staged_is_swept_once_it_is_old() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::init(dir.path()).unwrap();
        let file = |name: &str| dir.path().join(name);
        let (old, new, other) =
            (file("server.toml.4242-7.new"), file("server.toml.4242-8.new"), file("notes.new"));
        let sessions = file("sessions.toml.4242-9.new");
        for left in [&old, &new, &other, &sessions] {
            std::fs::write(left, "left behind\n").unwrap();
        }
        aged(&old);
        aged(&other);
        aged(&sessions);
        // A repository whose deletion was written goes, whatever its age:
        // a directory moved keeps the time it had.
        let deleted = [file("repos/.deleted-shop-5f0e"), file("archived/.deleted-old-5f0e")];
        // One made aside goes once it is a day old: a clone takes its time.
        let (abandoned, adding) = (file("repos/.adding-web-5f0e"), file("repos/.adding-site-5f0e"));
        for made in deleted.iter().chain([&abandoned, &adding]) {
            std::fs::create_dir_all(made.join(".kito")).unwrap();
        }
        // A directory's time is set through a handle Unix opens, and Windows does not.
        #[cfg(unix)]
        {
            let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
            std::fs::File::open(&abandoned).unwrap().set_modified(two_days).unwrap();
        }
        // When an archived one was archived, staged and never put in place.
        let (dated, dating) = (file("archived/old.json.4242-3.new"), file("archived/new.json.4242-4.new"));
        for staged in [&dated, &dating] {
            std::fs::write(staged, "{}").unwrap();
        }
        aged(&dated);
        server.sweep_staged();
        // One written just now is somebody's who is writing; what is not the server's stays.
        assert!(!old.exists() && new.exists() && other.exists() && !sessions.exists());
        assert!(deleted.iter().all(|deleted| !deleted.exists()));
        assert!(adding.exists());
        #[cfg(unix)]
        assert!(!abandoned.exists());
        assert!(!dated.exists() && dating.exists());
        assert!(Server::open(dir.path()).is_ok());
    }

    #[test]
    fn sessions_that_ended_are_not_written_back_by_one_opened_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        person(&mut server, "bob");
        person(&mut server, "carol");
        let server = std::sync::Arc::new(server);
        // No more of hers in all than a person has at most.
        let (rounds, threads, each) = (4, 3, 5);
        assert!(rounds * threads * each <= SESSIONS_EACH);
        for round in 0..rounds {
            for _ in 0..4 {
                server.open_session("bob", ("a token", None), |_| true).unwrap().unwrap();
            }
            // carol signs in over and over while bob's token is changed,
            // which ends his sessions.
            let opening: Vec<_> = (0..threads)
                .map(|_| {
                    let server = server.clone();
                    std::thread::spawn(move || {
                        for _ in 0..each {
                            server.open_session("carol", ("a token", None), |_| true).unwrap().unwrap();
                        }
                    })
                })
                .collect();
            let mut now = Server::open(dir.path()).unwrap();
            let (_, _, ended) = now.change_token("bob", None).unwrap();
            assert_eq!(ended, 4, "round {round}");
            for thread in opening {
                thread.join().unwrap();
            }
            assert_eq!(sessions_of(&server, "bob"), 0, "round {round}: they came back");
        }
        // And none of hers was lost to another written at the same time.
        assert_eq!(sessions_of(&server, "carol"), rounds * threads * each);
    }

    /// What was queued for the plugins, by the server's directory: each
    /// kind and event, in the order it was queued.
    pub(crate) fn queued() -> &'static std::sync::Mutex<Vec<(PathBuf, String, serde_json::Value)>> {
        static QUEUED: std::sync::OnceLock<std::sync::Mutex<Vec<(PathBuf, String, serde_json::Value)>>> =
            std::sync::OnceLock::new();
        QUEUED.get_or_init(Default::default)
    }

    #[test]
    fn a_name_is_taken_with_the_lock_held_and_an_archived_one_s_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::init(dir.path()).unwrap();
        // Somebody holds the lock: the name is not taken past them.
        let held = Lock::take(dir.path()).unwrap();
        let refused = server.add_repo("shop", None, None, false).unwrap_err();
        assert!(refused.contains("held by something that does not finish"), "{refused}");
        assert!(!dir.path().join("repos/shop").exists());
        drop(held);
        server.add_repo("shop", None, None, false).unwrap();
        // Archived, its name is its still.
        server.archive_repo("shop", Some("ada")).unwrap();
        let refused = server.add_repo("shop", None, None, false).unwrap_err();
        assert!(refused.contains("an archived repository is called `shop`"), "{refused}");
        assert!(!dir.path().join("repos/shop").exists());
        // What fails once the name is taken leaves nothing under it.
        let refused = server.add_repo("site", Some("/nowhere/at/all"), None, true).unwrap_err();
        assert!(refused.starts_with("git clone"), "{refused}");
        assert!(!dir.path().join("repos/site").exists());
    }

    #[test]
    fn a_name_deleted_is_nobody_s_kitchen_and_has_no_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        server.add_repo("shop", None, None, false).unwrap();
        let site = server.add_repo("site", None, None, false).unwrap();
        let store = site.join(kito_store::STORE_DIR);
        kitod::config::write_value(&store, "kitchen", "path", Some("../shop".into())).unwrap();
        server.note_setting("shop", "ada", "approvals", &serde_json::json!(1));
        assert_eq!(server.settings_history("shop").len(), 1);
        server.delete_repo("shop", None, None).unwrap();
        // A `shop` added later starts with nothing of this one.
        assert!(server.settings_history("shop").is_empty());
        assert!(!dir.path().join("settings/shop.jsonl").exists());
        let kitchen = kitod::config::Config::load(&store).unwrap().kitchen.path;
        assert_eq!(kitchen, None);
    }

    #[test]
    fn a_deletion_is_written_before_its_directory_is_moved_and_nothing_is_moved_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        person(&mut server, "bob");
        server.add_repo("shop", None, None, false).unwrap();
        server.grant("bob", "shop", Some(Role::Write)).unwrap();
        let aside = || {
            std::fs::read_dir(dir.path().join(REPOS_DIR))
                .unwrap()
                .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
                .filter(|name| name.starts_with(".deleted-"))
                .count()
        };
        let given = || Server::open(dir.path()).unwrap().config.users["bob"].access.get("shop").copied();
        // Not written: nothing is moved, and nothing is changed.
        let failing = |step: &str| dir.path().join(format!("fail-{step}-in-the-tests"));
        std::fs::write(failing("saving"), "").unwrap();
        assert_eq!(server.delete_repo("shop", None, None), Err(COULD_NOT.to_owned()));
        assert!(server.repo("shop").is_some() && aside() == 0 && given() == Some(Role::Write));
        std::fs::remove_file(failing("saving")).unwrap();
        // Written, and its directory not moved: that is said as it is.
        std::fs::write(failing("moving-aside"), "").unwrap();
        let said = server.delete_repo("shop", None, None).unwrap_err();
        assert!(said.contains("is given to nobody any more, but its directory could not be moved"), "{said}");
        assert!(said.ends_with("ask for the deletion again"), "{said}");
        assert!(server.repo("shop").is_some() && aside() == 0 && given().is_none());
        std::fs::remove_file(failing("moving-aside")).unwrap();
        // Asked again, it is deleted.
        assert_eq!(server.delete_repo("shop", None, None), Ok(Which::Live));
        assert!(server.repo("shop").is_none() && aside() == 0);
    }

    #[test]
    fn an_archived_repository_is_due_only_by_a_date_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        server.add_repo("shop", None, None, false).unwrap();
        // When it was archived cannot be written: it is not archived.
        let said = dir.path().join("archived/shop.json");
        std::fs::create_dir_all(&said).unwrap();
        assert!(server.archive_repo("shop", Some("ada")).is_err());
        assert!(server.repo("shop").is_some() && !dir.path().join("archived/shop").exists());
        assert_eq!(server.purge_archived(), Vec::<String>::new());
        std::fs::remove_dir(&said).unwrap();

        // Archived, and its date lost: it is not due, and is dated now.
        server.archive_repo("shop", Some("ada")).unwrap();
        std::fs::remove_file(&said).unwrap();
        assert_eq!(server.archived()[0]["deleted_at_ms"], serde_json::Value::Null);
        assert_eq!(server.purge_archived(), Vec::<String>::new());
        assert!(dir.path().join("archived/shop").is_dir());
        let dated = server.archived()[0]["deleted_at_ms"].as_u64().unwrap();
        assert!(dated > now_ms() + (KEEP_ARCHIVED_DAYS - 1) * 86_400_000, "{dated}");
        // The purge deletes only one that is due when it holds the lock.
        let refused = server.delete_repo_when("shop", Some(Which::Archived), None, true).unwrap_err();
        assert!(refused.contains("is not due to be deleted"), "{refused}");
        std::fs::write(&said, r#"{"at_ms": 0, "by": "ada"}"#).unwrap();
        assert_eq!(server.purge_archived(), ["shop"]);
    }

    #[test]
    fn told_waits_once_and_not_again_each_time_something_is_told() {
        // One that is never told, and others told one after another before it.
        let stuck = u64::MAX;
        waiting().lock().unwrap().insert(stuck, "stuck_in_the_tests".to_owned());
        let telling = std::thread::spawn(|| {
            for nth in 0..600u64 {
                let nth = u64::MAX - 1000 + nth;
                waiting().lock().unwrap().insert(nth, "told_in_the_tests".to_owned());
                waiting().lock().unwrap().remove(&(nth - 1));
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
            waiting().lock().unwrap().remove(&(u64::MAX - 1000 + 599));
        });
        let asked = std::time::Instant::now();
        told_within(std::time::Duration::from_millis(300));
        let took = asked.elapsed();
        waiting().lock().unwrap().remove(&stuck);
        telling.join().unwrap();
        assert!(took < std::time::Duration::from_secs(1), "{took:?}");
    }

    #[test]
    fn when_a_person_was_here_is_noted_without_waiting_for_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::init(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join(SEEN_DIR)).unwrap();
        let held = Lock::take(&dir.path().join(SEEN_DIR)).unwrap();
        let asked = std::time::Instant::now();
        server.saw("dora");
        assert!(asked.elapsed() < std::time::Duration::from_millis(500), "{:?}", asked.elapsed());
        assert_eq!(server.seen("dora"), None);
        drop(held);
        // Not noted then, it is at the next request.
        server.saw("dora");
        assert!(server.seen("dora").is_some());
    }

    /// What is beside the repositories in `dir` (`repos/` or `archived/`)
    /// and starts with `prefix`.
    fn beside(dir: &Path, prefix: &str) -> Vec<String> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
            .filter(|name| name.starts_with(prefix))
            .collect()
    }

    /// Whether any file under `dir` says `text`.
    fn says(dir: &Path, text: &str) -> bool {
        std::fs::read_dir(dir).into_iter().flatten().flatten().any(|entry| match entry.path().is_dir() {
            true => says(&entry.path(), text),
            false => std::fs::read(entry.path())
                .is_ok_and(|bytes| bytes.windows(text.len()).any(|window| window == text.as_bytes())),
        })
    }

    #[test]
    fn a_repository_is_made_aside_and_takes_its_name_when_it_is_whole() {
        let dir = tempfile::tempdir().unwrap();
        let server = std::sync::Arc::new(Server::init(dir.path()).unwrap());
        let repos = dir.path().join(REPOS_DIR);
        // Two add one name at once: one has it, the other is told it is
        // taken, and what it made aside is gone.
        let adding: Vec<_> = (0..2)
            .map(|_| {
                let server = server.clone();
                std::thread::spawn(move || server.add_repo("shop", None, None, false))
            })
            .collect();
        let added: Vec<Result<PathBuf, String>> = adding.into_iter().map(|one| one.join().unwrap()).collect();
        assert_eq!(added.iter().filter(|one| one.is_ok()).count(), 1, "{added:?}");
        let refused = added.iter().find_map(|one| one.clone().err()).unwrap();
        assert!(refused.contains("the server already has something called `shop`"), "{refused}");
        assert_eq!(beside(&repos, ".adding-"), Vec::<String>::new());
        assert_eq!(server.repos(), ["shop"]);
        // It keeps nothing of where it was made.
        assert!(!says(&repos.join("shop"), ".adding-"), "the repository names where it was made");

        // Half made, it is nobody: not listed, and not reached by its name.
        let mut server = std::sync::Arc::try_unwrap(server).ok().unwrap();
        let aside = server.adding_dir("site").unwrap();
        kitod::Repo::init(&aside).unwrap().close().unwrap();
        let hidden = aside.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(server.repos(), ["shop"]);
        assert!(server.repo("site").is_none() && server.repo(&hidden).is_none());
        assert!(server.delete_repo("site", None, None).is_err());
        assert!(server.delete_repo(&hidden, None, None).is_err());
        assert!(server.archive_repo("site", None).is_err() && server.archive_repo(&hidden, None).is_err());
        assert!(server.rename_repo("site", "web", None).is_err());
        assert!(server.rename_repo(&hidden, "web", None).is_err());
        assert!(aside.join(".kito/format").is_file(), "what is being made was touched");

        // Something has the name by the time it is whole: it is not given
        // it, and only what was made aside is taken away.
        std::fs::create_dir_all(repos.join("site")).unwrap();
        std::fs::write(repos.join("site/someone-else-s"), "kept").unwrap();
        let refused = server.take_name("site", &aside, Vec::new()).unwrap_err();
        assert!(refused.contains("the server already has something called `site`"), "{refused}");
        assert!(!aside.exists());
        assert_eq!(std::fs::read_to_string(repos.join("site/someone-else-s")).unwrap(), "kept");
        // A clone that fails leaves nothing.
        assert!(server.add_repo("web", Some("/nowhere/at/all"), None, true).is_err());
        assert_eq!(beside(&repos, ".adding-"), Vec::<String>::new());
        assert!(!repos.join("web").exists());
    }

    #[test]
    fn a_rename_that_is_not_written_is_put_back_with_the_lock_held() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        server.add_repo("shop", None, None, false).unwrap();
        let failing = |step: &str| dir.path().join(format!("fail-{step}-in-the-tests"));
        std::fs::write(failing("saving"), "").unwrap();
        assert_eq!(server.rename_repo("shop", "store", None), Err(COULD_NOT.to_owned()));
        assert!(server.repo("shop").is_some() && server.repo("store").is_none());
        // Put back it cannot be: that is said.
        std::fs::write(failing("moving-back"), "").unwrap();
        let said = server.rename_repo("shop", "store", None).unwrap_err();
        assert!(said.starts_with(COULD_NOT), "{said}");
        assert!(
            said.contains("its directory is still called `store`, and could not be called `shop` again"),
            "{said}"
        );
        assert!(server.repo("store").is_some());
    }

    #[test]
    fn a_repository_deleted_is_put_aside_beside_where_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let failing = |step: &str| dir.path().join(format!("fail-{step}-in-the-tests"));
        std::fs::write(failing("removing"), "").unwrap();
        server.add_repo("shop", None, None, false).unwrap();
        server.add_repo("old", None, None, false).unwrap();
        server.archive_repo("old", None).unwrap();
        server.delete_repo("shop", None, None).unwrap();
        server.delete_repo("old", None, None).unwrap();
        // Beside where each was, on the same disk: not in the server's directory.
        assert_eq!(beside(&dir.path().join(REPOS_DIR), ".deleted-shop-").len(), 1);
        assert_eq!(beside(&dir.path().join(ARCHIVED_DIR), ".deleted-old-").len(), 1);
        assert_eq!(beside(dir.path(), ".deleted-"), Vec::<String>::new());
        assert!(server.archived().is_empty() && server.repos().is_empty());
        server.sweep_staged();
        assert!(beside(&dir.path().join(REPOS_DIR), ".deleted-").is_empty());
        assert!(beside(&dir.path().join(ARCHIVED_DIR), ".deleted-").is_empty());

        // One whose directory could not be moved is said so in the log.
        std::fs::remove_file(failing("removing")).unwrap();
        server.add_repo("site", None, None, false).unwrap();
        std::fs::write(failing("moving-aside"), "").unwrap();
        assert!(server.delete_repo("site", None, Some("olga")).is_err());
        let filter = crate::audit::Filter {
            kind: Some("repo_not_deleted".to_owned()),
            limit: 5,
            ..Default::default()
        };
        let kept = crate::audit::read(dir.path(), &filter);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0]["by"], "olga");
        assert_eq!(kept[0]["what"], "site: nobody is given it any more, and its directory is still there");
    }

    #[test]
    fn what_happens_to_a_repository_is_told_by_what_does_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = Server::init(dir.path()).unwrap();
        let added = vec![("repo_added", serde_json::json!({ "repo": "shop", "what": "shop" }))];
        server.add_repo_told("shop", None, None, false, added).unwrap();
        server.rename_repo("shop", "store", Some("ada")).unwrap();
        server.archive_repo("store", Some("ada")).unwrap();
        server.unarchive_repo("store", Some("olga")).unwrap();
        server.delete_repo("store", None, Some("olga")).unwrap();
        let here = server.dir.clone();
        let queued: Vec<String> = queued()
            .lock()
            .unwrap()
            .iter()
            .filter(|(dir, _, _)| *dir == here)
            .map(|(_, kind, event)| format!("{kind} {}", event["what"].as_str().unwrap_or_default()))
            .collect();
        assert_eq!(
            queued,
            [
                "repo_added shop",
                "repo_renamed shop is store now",
                "repo_archived store, deleted in 30 days",
                "repo_unarchived store",
                "repo_deleted store",
            ]
        );
        told();
    }
}
