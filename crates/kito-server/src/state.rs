//! What a server keeps: its configuration, its people, its repositories.

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
/// Days an archived repository is kept.
pub const KEEP_ARCHIVED_DAYS: u64 = 30;
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
}

const SESSIONS_FILE: &str = "sessions.toml";
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

pub struct Server {
    pub dir: PathBuf,
    pub config: Config,
}

/// What a token is kept as.
pub fn hash_of(token: &str) -> String {
    kito_core::Hash::of(token.as_bytes()).to_string()
}

/// A name a repository may have (ADR 0018).
pub fn check_repo_name(name: &str) -> Result<(), String> {
    check_name("repository", name)
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

impl Server {
    pub fn init(dir: &Path) -> Result<Server, String> {
        std::fs::create_dir_all(dir.join(REPOS_DIR)).map_err(|e| format!("{}: {e}", dir.display()))?;
        let dir = dir.canonical().map_err(|e| format!("{}: {e}", dir.display()))?;
        if dir.join(CONFIG_FILE).exists() {
            return Err(format!("{} is a server already", dir.display()));
        }
        let server = Server { dir, config: Config::default() };
        server.save()?;
        Ok(server)
    }

    pub fn open(dir: &Path) -> Result<Server, String> {
        let dir = dir.canonical().map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(CONFIG_FILE);
        let text = std::fs::read_to_string(&path).map_err(|_| {
            format!("{} is not a Kito server (run `kito-server init` there, or give --dir)", dir.display())
        })?;
        let config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut server = Server { dir, config };
        // Written before ADR 0014: read as it was meant, written back so.
        let all = server.repos();
        let mut changed = false;
        for user in server.config.users.values_mut() {
            changed |= user.read_as_meant(&all);
        }
        if changed {
            server.save()?;
        }
        Ok(server)
    }

    /// Writes the configuration: it holds what tokens hash to, and is
    /// readable by its owner only.
    pub fn save_config(&self) -> Result<(), String> {
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        use std::io::Write;
        let path = self.dir.join(CONFIG_FILE);
        let text = toml::to_string(&self.config).map_err(|e| e.to_string())?;
        let staged = path.with_extension("toml.new");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // On Windows the user profile's permissions keep it from other users (ADR 0002).
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options
            .open(&staged)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .and_then(|()| std::fs::rename(&staged, &path));
        written.map_err(|e| format!("{}: {e}", path.display()))
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
    pub fn archive_repo(&self, name: &str, by: &str) -> Result<(), String> {
        let root =
            self.repo(name).ok_or_else(|| format!("there is no repository `{name}` on this server"))?;
        let to = self.archived_dir().join(name);
        if to.exists() {
            return Err(format!("an archived repository is called `{name}` already: delete it first"));
        }
        Self::stop_daemon(&root);
        std::fs::create_dir_all(self.archived_dir()).map_err(|e| e.to_string())?;
        std::fs::rename(&root, &to).map_err(|e| format!("{}: {e}", root.display()))?;
        let said = serde_json::json!({ "at_ms": now_ms(), "by": by });
        std::fs::write(self.archived_dir().join(format!("{name}.json")), said.to_string())
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The archived repositories: name, when, by whom, and when it goes.
    pub fn archived(&self) -> Vec<serde_json::Value> {
        let mut found: Vec<serde_json::Value> = std::fs::read_dir(self.archived_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let said: serde_json::Value =
                    std::fs::read_to_string(self.archived_dir().join(format!("{name}.json")))
                        .ok()
                        .and_then(|text| serde_json::from_str(&text).ok())
                        .unwrap_or_default();
                let at = said["at_ms"].as_u64().unwrap_or(0);
                Some(serde_json::json!({
                    "name": name,
                    "at_ms": at,
                    "by": said["by"],
                    "deleted_at_ms": at + KEEP_ARCHIVED_DAYS * 86_400_000,
                }))
            })
            .collect();
        found.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        found
    }

    /// Brings an archived repository back, as it was.
    pub fn unarchive_repo(&self, name: &str) -> Result<(), String> {
        check_name("repository", name)?;
        let from = self.archived_dir().join(name);
        if !from.is_dir() {
            return Err(format!("no archived repository is called `{name}`"));
        }
        let to = self.dir.join(REPOS_DIR).join(name);
        if to.exists() {
            return Err(format!("the server has a repository `{name}` already: rename one first"));
        }
        std::fs::rename(&from, &to).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(self.archived_dir().join(format!("{name}.json")));
        Ok(())
    }

    /// Deletes a repository for good, archived or not, and what the people
    /// were given of it. The backups still have it.
    pub fn delete_repo(&mut self, name: &str) -> Result<(), String> {
        check_name("repository", name)?;
        let (live, archived) = (self.repo(name), self.archived_dir().join(name));
        match (live, archived.is_dir()) {
            (Some(root), _) => {
                Self::stop_daemon(&root);
                std::fs::remove_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
            }
            (None, true) => {
                std::fs::remove_dir_all(&archived).map_err(|e| format!("{}: {e}", archived.display()))?;
                let _ = std::fs::remove_file(self.archived_dir().join(format!("{name}.json")));
            }
            (None, false) => return Err(format!("there is no repository `{name}` on this server")),
        }
        for user in self.config.users.values_mut() {
            user.access.remove(name);
        }
        self.config.renamed.retain(|old, new| old != name && new != name);
        self.save()
    }

    /// Deletes the archived repositories kept longer than they are kept.
    /// Answers which.
    pub fn purge_archived(&mut self) -> Vec<String> {
        let now = now_ms();
        let due: Vec<String> = self
            .archived()
            .into_iter()
            .filter(|one| one["deleted_at_ms"].as_u64().is_some_and(|at| at <= now))
            .filter_map(|one| one["name"].as_str().map(str::to_owned))
            .collect();
        due.into_iter().filter(|name| self.delete_repo(name).is_ok()).collect()
    }

    /// Renames a repository: who may use it, the kitchens that name it and
    /// what was kept of its settings follow; the old name still reaches it.
    pub fn rename_repo(&mut self, from: &str, to: &str) -> Result<(), String> {
        check_name("repository", to)?;
        let root =
            self.repo(from).ok_or_else(|| format!("there is no repository `{from}` on this server"))?;
        let target = self.dir.join(REPOS_DIR).join(to);
        if target.exists() || self.archived_dir().join(to).exists() {
            return Err(format!("the server has a repository `{to}` already"));
        }
        Self::stop_daemon(&root);
        std::fs::rename(&root, &target).map_err(|e| e.to_string())?;
        for user in self.config.users.values_mut() {
            if let Some(role) = user.access.remove(from) {
                user.access.insert(to.to_owned(), role);
            }
        }
        // An old name taken by the new one is no longer an old name.
        self.config.renamed.remove(to);
        for new in self.config.renamed.values_mut() {
            if new == from {
                *new = to.to_owned();
            }
        }
        self.config.renamed.insert(from.to_owned(), to.to_owned());
        self.save()?;
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
    pub fn add_repo_from_git(&self, name: &str, url: &str, token: Option<&str>) -> Result<PathBuf, String> {
        let token = token.map(str::trim).filter(|token| !token.is_empty());
        let given = match token {
            Some(token) => match url.split_once("://") {
                Some(("https", rest)) => format!("https://x-access-token:{token}@{rest}"),
                _ => return Err("a token goes with an https:// address".into()),
            },
            None => url.to_owned(),
        };
        let added = self.add_repo(name, Some(&given), true).map_err(|why| match token {
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

    /// Creates a repository: empty, or with the history of a git one. One
    /// that comes with a history is protected unless told otherwise: its
    /// main lane changes only when a lane is integrated.
    pub fn add_repo(&self, name: &str, from: Option<&str>, protect: bool) -> Result<PathBuf, String> {
        check_name("repository", name)?;
        let root = self.dir.join(REPOS_DIR).join(name);
        if root.exists() {
            return Err(format!("the server already has something called `{name}`"));
        }
        let fail = |e: &dyn std::fmt::Display| format!("{name}: {e}");
        match from {
            Some(from) => {
                // A directory here is given to git as a plain path: git on
                // Windows does not take one that starts with `\\?\` (ADR 0002).
                let local = Path::new(from).canonical().ok().filter(|path| path.is_dir());
                let from = local.as_ref().and_then(|path| path.to_str()).unwrap_or(from);
                let cloned = std::process::Command::new("git")
                    .args(["clone", "--quiet", from])
                    .arg(&root)
                    .output()
                    .map_err(|e| format!("git: {e}"))?;
                if !cloned.status.success() {
                    let _ = std::fs::remove_dir_all(&root);
                    return Err(format!(
                        "git clone {from}: {}",
                        String::from_utf8_lossy(&cloned.stderr).trim()
                    ));
                }
                // Kito's own directory is not git's business.
                let exclude = root.join(".git").join("info").join("exclude");
                let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
                text.push_str("\n.kito/\n");
                let _ = std::fs::write(&exclude, text);
                let mut repo = kitod::Repo::init(&root).map_err(|e| fail(&e))?;
                repo.import_git().map_err(|e| fail(&e))?;
                repo.scan().map_err(|e| fail(&e))?;
                if protect {
                    repo.protect(true).map_err(|e| fail(&e))?;
                }
                repo.close().map_err(|e| fail(&e))?;
            }
            None => {
                std::fs::create_dir_all(&root).map_err(|e| fail(&e))?;
                kitod::Repo::init(&root).map_err(|e| fail(&e))?.close().map_err(|e| fail(&e))?;
            }
        }
        Ok(root)
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
            return Err(format!("{name} is a user already: remove them first to give them a new token"));
        }
        for (repo, _) in access {
            if self.repo(repo).is_none() {
                return Err(format!("there is no repository `{repo}` on this server"));
            }
        }
        let mut bytes = [0u8; 32];
        // The system's randomness, on every platform (ADR 0002).
        getrandom::getrandom(&mut bytes).map_err(|e| format!("no randomness to make a token from: {e}"))?;
        let token = format!("kito_{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
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
            logins: identity
                .iter()
                .map(|identity| format!("{}:{}", identity.provider, identity.login))
                .collect(),
            ..User::default()
        };
        self.config.users.insert(name.to_owned(), user);
        self.save()?;
        Ok(identity.is_none().then_some(token))
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
        let dir = self.dir.join(SEEN_DIR);
        if check_name("user", name).is_err() || std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let staged = dir.join(format!(".{name}.new"));
        if std::fs::write(&staged, now_ms().to_string()).is_ok() {
            let _ = std::fs::rename(&staged, dir.join(name));
        }
    }

    /// Notes that `by` set a repository's `setting` to `value`.
    pub fn note_setting(&self, repo: &str, by: &str, setting: &str, value: &serde_json::Value) {
        let dir = self.dir.join(SETTINGS_DIR);
        if check_name("repository", repo).is_err() || std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join(format!("{repo}.jsonl"));
        let mut lines: Vec<String> =
            std::fs::read_to_string(&path).unwrap_or_default().lines().map(str::to_owned).collect();
        let line = serde_json::json!({ "at_ms": now_ms(), "by": by, "setting": setting, "value": value });
        lines.push(line.to_string());
        let keep = lines.len().saturating_sub(SETTINGS_KEPT);
        let staged = dir.join(format!(".{repo}.new"));
        if std::fs::write(&staged, lines[keep..].join("\n") + "\n").is_ok() {
            let _ = std::fs::rename(&staged, &path);
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
    pub fn set_server_role(&mut self, name: &str, role: ServerRole) -> Result<(), String> {
        let user =
            self.config.users.get_mut(name).ok_or_else(|| format!("{name} is not a user of this server"))?;
        user.server_role = role;
        self.save()
    }

    /// Gives a person a repository, with a role in it; `None` takes it
    /// away. Answers whether anything changed.
    pub fn grant(&mut self, name: &str, repo: &str, role: Option<Role>) -> Result<bool, String> {
        if self.repo(repo).is_none() {
            return Err(format!("there is no repository `{repo}` on this server"));
        }
        let user =
            self.config.users.get_mut(name).ok_or_else(|| format!("{name} is not a user of this server"))?;
        let before = user.access.get(repo).copied();
        match role {
            Some(role) => drop(user.access.insert(repo.to_owned(), role)),
            None => drop(user.access.remove(repo)),
        }
        let changed = before != user.access.get(repo).copied();
        if changed {
            self.save()?;
        }
        Ok(changed)
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

    pub fn remove_user(&mut self, name: &str) -> Result<(), String> {
        if self.config.users.remove(name).is_none() {
            return Err(format!("{name} is not a user of this server"));
        }
        let _ = std::fs::remove_file(self.dir.join(SEEN_DIR).join(name));
        self.save()?;
        // Their sessions end with them.
        self.change_sessions(|sessions| sessions.retain(|_, session| session.user != name))
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
        let who = format!("{}:{}", identity.provider, identity.login);
        match self.user_by_identity(identity) {
            Some(name) => {
                self.tell("signed_in", serde_json::json!({ "user": name, "identity": identity.key(), "what": format!("{name} ({who})") }));
                Ok(name)
            }
            None => {
                self.tell(
                    "sign_in_refused",
                    serde_json::json!({ "identity": identity.key(), "what": who.clone() }),
                );
                // The same answer for whoever it is: nothing about who is known.
                Err(format!(
                    "{who} is nobody on this server: an administrator adds people (`kito-server user add <name> \
                     --identity {who}`)"
                ))
            }
        }
    }

    fn sessions(&self) -> Sessions {
        std::fs::read_to_string(self.dir.join(SESSIONS_FILE))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Changes the sessions and writes them, readable by the server's
    /// user only; the ones that ended are dropped on the way.
    fn change_sessions(&self, change: impl FnOnce(&mut BTreeMap<String, Session>)) -> Result<(), String> {
        use std::io::Write;
        let mut all = self.sessions();
        let now = now_ms();
        all.sessions.retain(|_, session| session.until_ms > now);
        change(&mut all.sessions);
        let path = self.dir.join(SESSIONS_FILE);
        let text = toml::to_string(&all).map_err(|e| e.to_string())?;
        let staged = path.with_extension("toml.new");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .open(&staged)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .and_then(|()| std::fs::rename(&staged, &path))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Opens a session for a user and returns its token, which is not
    /// kept: it ends after `[sign_in] days`.
    pub fn open_session(&self, name: &str) -> Result<String, String> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|e| format!("no randomness to make a session from: {e}"))?;
        let token =
            format!("{SESSION_PREFIX}{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
        let until_ms = now_ms() + self.config.sign_in.days * 24 * 3600 * 1000;
        let session = Session { user: name.to_owned(), until_ms };
        self.change_sessions(|sessions| drop(sessions.insert(hash_of(&token), session)))?;
        Ok(token)
    }

    /// Ends a session, and tells the plugins that listen whose it was.
    /// Nothing happens for one that is not there.
    pub fn end_session(&self, token: &str) {
        let hash = hash_of(token);
        let Some(session) = self.sessions().sessions.get(&hash).cloned() else { return };
        if self.change_sessions(|sessions| drop(sessions.remove(&hash))).is_ok() {
            self.tell("session_ended", serde_json::json!({ "user": session.user, "what": session.user }));
        }
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
        self.config.plugins.insert(name.to_owned(), config);
        self.save()?;
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
        let was = self.config.plugins.get_mut(name).filter(|plugin| plugin.enabled).map(|plugin| {
            plugin.enabled = false;
        });
        let _ = std::fs::remove_file(self.failure_file(name));
        // What it kept of its own here stays for when it is enabled
        // again, unless asked otherwise.
        if purge && kitod::plugins::check_name("plugin", name).is_ok() {
            let _ = std::fs::remove_dir_all(self.plugin_dir(name));
        }
        match was {
            Some(()) => self.save().map(|()| true),
            None => Ok(false),
        }
    }

    /// Forgets a plugin that is not enabled: the server says nothing of
    /// it any more. `Err`: it is enabled.
    pub fn forget_plugin(&mut self, name: &str) -> Result<(), String> {
        if self.config.plugins.get(name).is_some_and(|plugin| plugin.enabled) {
            return Err(format!(
                "the plugin `{name}` is enabled on this server: `kito-server plugin disable {name}` first"
            ));
        }
        if self.config.plugins.remove(name).is_some() {
            self.save()?;
        }
        Ok(())
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
    pub fn tell(&self, kind: &str, mut event: serde_json::Value) {
        use kitod::plugins::Permission;
        // What changes the server is kept, whoever listens (ADR 0017).
        let said = |key: &str| event.get(key).and_then(serde_json::Value::as_str).map(str::to_owned);
        let what = said("what").unwrap_or_else(|| event.to_string());
        // Who did it: whoever the event says; for a sign-in or a session, the person themselves.
        let by = match kind {
            "signed_in" | "session_ended" | "session_opened" => said("user"),
            _ => said("by"),
        };
        crate::audit::record(&self.dir, kind, by.as_deref(), said("repo").as_deref(), &what);
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
                        eprintln!("kito-server: the plugin `{name}` failed {what}");
                        let _ = std::fs::create_dir_all(self.plugin_dir(name));
                        let _ = std::fs::write(self.failure_file(name), &what);
                        self.tell("plugin_failed", serde_json::json!({ "plugin": name, "what": what }));
                    }
                }
            }
        }
    }

    /// The person a token belongs to.
    pub fn user_of(&self, token: &str) -> Option<(String, User)> {
        let hash = hash_of(token);
        // A session: of somebody who signed in, until it ends.
        if token.starts_with(SESSION_PREFIX) {
            let session =
                self.sessions().sessions.get(&hash).cloned().filter(|session| session.until_ms > now_ms())?;
            let user = self.config.users.get(&session.user)?.clone();
            return Some((session.user, user));
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
