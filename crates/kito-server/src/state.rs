//! What a server keeps: its configuration, its people, its repositories.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "server.toml";
const REPOS_DIR: &str = "repos";
/// The address a new server listens on: this machine only. Clients on
/// other machines come in through SSH, or the address is changed by whoever
/// knows the network is theirs.
const DEFAULT_LISTEN: &str = "127.0.0.1:7421";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address `serve` listens on.
    pub listen: String,
    pub users: BTreeMap<String, User>,
}

impl Default for Config {
    fn default() -> Self {
        Config { listen: DEFAULT_LISTEN.to_owned(), users: BTreeMap::new() }
    }
}

/// What a person may do in a repository. Each role can do what the ones
/// before it can.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
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
}

impl Role {
    pub fn word(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Integrate => "integrate",
        }
    }

    fn is_default(&self) -> bool {
        *self == Role::default()
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
    /// The repositories they may use; `*` is all of them.
    pub repos: Vec<String>,
    /// What they may do in them.
    #[serde(skip_serializing_if = "Role::is_default")]
    pub role: Role,
    /// What they may do in the repositories where it is something else.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, Role>,
}

impl User {
    pub fn may_use(&self, repo: &str) -> bool {
        self.repos.iter().any(|allowed| allowed == "*" || allowed == repo)
    }

    /// What the person may do in a repository.
    pub fn role_in(&self, repo: &str) -> Role {
        self.roles.get(repo).copied().unwrap_or(self.role)
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
        let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
        if dir.join(CONFIG_FILE).exists() {
            return Err(format!("{} is a server already", dir.display()));
        }
        let server = Server { dir, config: Config::default() };
        server.save()?;
        Ok(server)
    }

    pub fn open(dir: &Path) -> Result<Server, String> {
        let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(CONFIG_FILE);
        let text = std::fs::read_to_string(&path).map_err(|_| {
            format!("{} is not a Kito server (run `kito-server init` there, or give --dir)", dir.display())
        })?;
        let config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Server { dir, config })
    }

    /// Writes the configuration: it holds what tokens hash to, and is
    /// readable by its owner only.
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

    /// Adds a person and returns their token, which is not kept.
    pub fn add_user(
        &mut self,
        name: &str,
        email: Option<String>,
        repos: &[String],
        role: Role,
    ) -> Result<String, String> {
        check_name("user", name)?;
        if self.config.users.contains_key(name) {
            return Err(format!("{name} is a user already: remove them first to give them a new token"));
        }
        for repo in repos {
            if repo != "*" && self.repo(repo).is_none() {
                return Err(format!("there is no repository `{repo}` on this server"));
            }
        }
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut source| source.read_exact(&mut bytes))
            .map_err(|e| format!("no randomness to make a token from: {e}"))?;
        let token = format!("kito_{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
        let repos = match repos.is_empty() {
            true => vec!["*".to_owned()],
            false => repos.to_vec(),
        };
        let user = User { email, token: hash_of(&token), repos, role, roles: BTreeMap::new() };
        self.config.users.insert(name.to_owned(), user);
        self.save()?;
        Ok(token)
    }

    /// Changes what a person may do: everywhere, or in one repository.
    pub fn set_role(&mut self, name: &str, role: Role, repo: Option<&str>) -> Result<(), String> {
        if let Some(repo) = repo.filter(|repo| self.repo(repo).is_none()) {
            return Err(format!("there is no repository `{repo}` on this server"));
        }
        let user =
            self.config.users.get_mut(name).ok_or_else(|| format!("{name} is not a user of this server"))?;
        match repo {
            Some(repo) => drop(user.roles.insert(repo.to_owned(), role)),
            None => {
                user.role = role;
                user.roles.clear();
            }
        }
        self.save()
    }

    pub fn remove_user(&mut self, name: &str) -> Result<(), String> {
        if self.config.users.remove(name).is_none() {
            return Err(format!("{name} is not a user of this server"));
        }
        self.save()
    }

    /// The person a token belongs to.
    pub fn user_of(&self, token: &str) -> Option<(String, User)> {
        let hash = hash_of(token);
        self.config
            .users
            .iter()
            .find(|(_, user)| user.token == hash)
            .map(|(name, user)| (name.clone(), user.clone()))
    }
}
