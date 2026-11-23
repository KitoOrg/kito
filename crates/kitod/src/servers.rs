//! The servers a person is connected to (ADR 0023).
//!
//! They are kept in `servers.toml`, in the person's Kito directory: how
//! each is reached, how it is known to be the one meant, who the person is
//! there and what they sign in with; and which one is current. A directory
//! connected to a repository names one of them in `.kito/remote.toml`, so
//! a session renewed once serves every directory of that server.
//!
//! The file holds tokens: it is readable by its owner only, and is always
//! written whole to a file beside it, then renamed over it. Whatever changes
//! it holds `servers.lock` meanwhile.

use crate::client::{Remote, REMOTE_FILE};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub use crate::plugins::config_dir;

/// The file of the servers a person is connected to, in their Kito directory.
pub const SERVERS_FILE: &str = "servers.toml";

/// Held by whatever changes `servers.toml`.
const LOCK_FILE: &str = "servers.lock";

/// The port of a Kito server when an address says none.
pub const DEFAULT_PORT: u16 = 7421;

/// What a store set aside is called when its directory is connected to
/// the repository it was sent as: `.kito-local-<UTC date-time>`.
pub const LOCAL_BACKUP_PREFIX: &str = ".kito-local-";

/// A server, as a person is connected to it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    /// `host:port`, for a server reached over TCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// A command whose standard input and output speak the protocol:
    /// `ssh user@host kito-server --dir /srv/kito stdio`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// The fingerprint of the server's certificate (`sha256:…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The server is known by its name: a public authority vouches for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub authority: bool,
    /// The server does not encrypt (on this machine only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plain: bool,
    /// Who the person is there, as the server last said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The person's token, or a session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// When the token is a session's: the way another is got (`github`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<String>,
}

/// Never the token: whatever prints a server prints no secret.
impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("address", &self.address)
            .field("command", &self.command)
            .field("fingerprint", &self.fingerprint)
            .field("authority", &self.authority)
            .field("plain", &self.plain)
            .field("user", &self.user)
            .field("token", &self.token.as_ref().map(|_| "…"))
            .field("sign_in", &self.sign_in)
            .finish()
    }
}

impl Server {
    /// The server a connection reaches, with what it is reached with.
    pub fn of(remote: &Remote) -> Server {
        Server {
            address: remote.server.clone(),
            command: remote.command.clone(),
            fingerprint: remote.fingerprint.clone(),
            authority: remote.authority,
            plain: remote.plain,
            user: None,
            token: remote.token.clone(),
            sign_in: remote.sign_in.clone(),
        }
    }

    /// The connection to a repository of this server (`repo`: none, for
    /// what is asked of the server itself), as the entry `name`.
    pub fn remote(&self, name: &str, repo: &str) -> Remote {
        Remote {
            server: self.address.clone(),
            command: self.command.clone(),
            repo: repo.to_owned(),
            token: self.token.clone(),
            fingerprint: self.fingerprint.clone(),
            authority: self.authority,
            plain: self.plain,
            sign_in: self.sign_in.clone(),
            name: Some(name.to_owned()),
        }
    }

    /// Where the server is, in words.
    pub fn place(&self) -> String {
        match &self.address {
            Some(address) => address.clone(),
            None => self.command.join(" "),
        }
    }

    /// Whether two entries reach the same server.
    pub fn same_place(&self, other: &Server) -> bool {
        self.address == other.address && self.command == other.command
    }

    /// Whether two entries are one connection: the same server, known the
    /// same way, with the same token and way to sign in. Only then does a
    /// directory acting through one act as the same person through the
    /// other.
    pub fn same_connection(&self, other: &Server) -> bool {
        self.same_place(other)
            && self.token == other.token
            && self.sign_in == other.sign_in
            && self.fingerprint == other.fingerprint
            && self.authority == other.authority
            && self.plain == other.plain
    }

    /// The name a server gets when none is given: its host as typed,
    /// without the port when it is the usual one; for SSH, `user@host`.
    pub fn name(&self) -> String {
        match (&self.address, self.command.as_slice()) {
            (Some(address), _) => name_of_address(address),
            (None, [ssh, host, ..]) if ssh == "ssh" => host.clone(),
            (None, command) => command.join(" "),
        }
    }

    /// Whether the token is a session's: renewed by signing in, never
    /// replaced by a person's token.
    pub fn holds_session(&self) -> bool {
        self.sign_in.is_some() || self.token.as_deref().is_some_and(|token| token.starts_with(SESSION_PREFIX))
    }
}

/// What a session's token starts with, as servers make them.
pub const SESSION_PREFIX: &str = "kitos_";

/// `host` for `host:7421`, `host:7500` for itself.
pub fn name_of_address(address: &str) -> String {
    match address.rsplit_once(':') {
        Some((host, port))
            if port.parse::<u16>() == Ok(DEFAULT_PORT) && (!host.contains(':') || host.ends_with(']')) =>
        {
            host.to_owned()
        }
        _ => address.to_owned(),
    }
}

/// What `servers.toml` holds.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Servers {
    /// The server a command that names none, outside a connected
    /// directory, asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, Server>,
}

impl Servers {
    /// What the person's Kito directory keeps: nothing when it has no
    /// such file, or one that cannot be read.
    pub fn load() -> Servers {
        config_dir().and_then(|dir| Servers::read_in(&dir).ok()).unwrap_or_default()
    }

    /// What `dir` keeps; an error for a file that is there and cannot be
    /// read, which is never written over.
    pub fn read_in(dir: &Path) -> Result<Servers, String> {
        let path = dir.join(SERVERS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| {
                format!("{} cannot be read ({}): put it right, or move it away", path.display(), e.message())
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Servers::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Changes what is kept in the person's Kito directory, under its
    /// lock, and writes it whole; nothing is written when `change` fails.
    pub fn change<T>(change: impl FnOnce(&mut Servers) -> Result<T, String>) -> Result<T, String> {
        Servers::change_in(&kito_dir()?, change)
    }

    pub fn change_in<T>(
        dir: &Path,
        change: impl FnOnce(&mut Servers) -> Result<T, String>,
    ) -> Result<T, String> {
        let _held = hold(dir)?;
        let mut servers = Servers::read_in(dir)?;
        let done = change(&mut servers)?;
        servers.write_in(dir)?;
        Ok(done)
    }

    fn write_in(&self, dir: &Path) -> Result<(), String> {
        let path = dir.join(SERVERS_FILE);
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        write_whole(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The name of the entry that reaches the same server, if one does.
    pub fn named_at(&self, server: &Server) -> Option<&str> {
        self.servers.iter().find(|(_, known)| known.same_place(server)).map(|(name, _)| name.as_str())
    }

    /// A name for a new entry: `wanted`, or `wanted-2`… when that is
    /// another server's.
    pub fn free_name(&self, wanted: &str) -> String {
        (1..)
            .map(|n| if n == 1 { wanted.to_owned() } else { format!("{wanted}-{n}") })
            .find(|name| !self.servers.contains_key(name))
            .unwrap_or_else(|| wanted.to_owned())
    }
}

/// The person's Kito directory, made if it is not there yet.
fn kito_dir() -> Result<PathBuf, String> {
    let dir = config_dir().ok_or("no place for a personal configuration: set HOME or KITO_CONFIG_DIR")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Holds the lock of `servers.toml` until the file answered is dropped.
/// The system lets it go if the process ends.
fn hold(dir: &Path) -> Result<std::fs::File, String> {
    let path = dir.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.lock().map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(file)
}

/// Writes a file whole: to a file beside it, readable by its owner only,
/// then renamed over it. Whoever reads it finds the one before or this
/// one, whatever happens meanwhile.
pub fn write_whole(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let beside = path.with_file_name(format!(".{name}.{}.new", std::process::id()));
    // One a process of the same number left: its mode is not to be kept.
    let _ = std::fs::remove_file(&beside);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // On Windows the user profile's permissions keep it from other users (ADR 0002).
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&beside).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    match written.and_then(|()| std::fs::rename(&beside, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&beside);
            Err(e)
        }
    }
}

/// What moving a directory to the form that names its server did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adopted {
    /// The entry the directory names now.
    pub name: String,
    /// The entry of the same server, for another account or known another
    /// way, that the name was not taken from: why it is not the host's.
    pub beside: Option<String>,
}

/// Moves a directory connected before 0.40, which keeps an address and a
/// token of its own, to the form that names its server (ADR 0023). An
/// entry is used only when it is the very same connection: the same
/// server, known the same way, the same token and way to sign in; a
/// directory never changes who it acts as. Otherwise its connection is
/// added under its host's name, or a free one (`host-2`) when that is
/// taken. Nothing becomes current. Then `remote.toml` names the entry.
/// `None`: the directory was not connected that way.
///
/// `servers.toml` is written first: a machine that stops in between
/// leaves the directory as it was, and the next command finds the entry.
/// Two commands at once, in two directories of a server, take turns.
pub fn adopt(root: &Path) -> Result<Option<Adopted>, String> {
    if !Remote::read(root).is_some_and(|remote| !remote.names_its_server()) {
        return Ok(None);
    }
    adopt_in(root, &kito_dir()?)
}

pub fn adopt_in(root: &Path, dir: &Path) -> Result<Option<Adopted>, String> {
    let _held = hold(dir)?;
    // Read again, held: another command may have moved it meanwhile.
    let Some(remote) = Remote::read(root).filter(|remote| !remote.names_its_server()) else {
        return Ok(None);
    };
    let mut servers = Servers::read_in(dir)?;
    let mut entry = Server::of(&remote);
    let same = servers.servers.iter().find(|(_, known)| known.same_connection(&entry));
    let adopted = match same {
        Some((name, _)) => Adopted { name: name.clone(), beside: None },
        None => {
            let wanted = entry.name();
            let name = servers.free_name(&wanted);
            // Who it is there, when an entry of the same token says so.
            entry.user = servers
                .servers
                .values()
                .find(|known| known.same_place(&entry) && known.token == entry.token && known.token.is_some())
                .and_then(|known| known.user.clone());
            let beside = (name != wanted).then_some(wanted);
            servers.servers.insert(name.clone(), entry);
            servers.write_in(dir)?;
            Adopted { name, beside }
        }
    };
    let file = root.join(kito_store::STORE_DIR).join(REMOTE_FILE);
    Remote::save_named(root, &adopted.name, &remote.repo).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(Some(adopted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_is_named_by_its_host_as_typed() {
        assert_eq!(name_of_address("sandbox.kito.sh:7421"), "sandbox.kito.sh");
        assert_eq!(name_of_address("127.0.0.1:7500"), "127.0.0.1:7500");
        assert_eq!(name_of_address("[::1]:7421"), "[::1]");
        assert_eq!(name_of_address("::1"), "::1");
        let ssh = Server {
            command: ["ssh", "ada@host", "kito-server", "--dir", "/srv/kito", "stdio"]
                .map(String::from)
                .to_vec(),
            ..Server::default()
        };
        assert_eq!(ssh.name(), "ada@host");
    }

    #[test]
    fn what_is_kept_is_written_whole_for_its_owner_and_never_printed() {
        let tmp = tempfile::tempdir().unwrap();
        Servers::change_in(tmp.path(), |servers| {
            let server = Server {
                address: Some("sandbox.kito.sh:7421".into()),
                authority: true,
                user: Some("ada".into()),
                token: Some("kito_secret".into()),
                ..Server::default()
            };
            servers.servers.insert(server.name(), server);
            servers.current = Some("sandbox.kito.sh".into());
            Ok(())
        })
        .unwrap();
        let text = std::fs::read_to_string(tmp.path().join(SERVERS_FILE)).unwrap();
        assert_eq!(
            text,
            "current = \"sandbox.kito.sh\"\n\n[servers.\"sandbox.kito.sh\"]\naddress = \
             \"sandbox.kito.sh:7421\"\nauthority = true\nuser = \"ada\"\ntoken = \"kito_secret\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(tmp.path().join(SERVERS_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Nothing is left beside it.
        let names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|name| name == SERVERS_FILE || name == LOCK_FILE), "{names:?}");
        let servers = Servers::read_in(tmp.path()).unwrap();
        assert!(!format!("{servers:?}").contains("kito_secret"));
        // One that cannot be read is said, and never written over.
        std::fs::write(tmp.path().join(SERVERS_FILE), "current = [").unwrap();
        assert!(Servers::change_in(tmp.path(), |_| Ok(())).is_err());
        assert_eq!(std::fs::read_to_string(tmp.path().join(SERVERS_FILE)).unwrap(), "current = [");
    }

    #[test]
    fn a_directory_connected_the_old_way_comes_to_name_its_server() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, one, two) = (tmp.path().join("me"), tmp.path().join("one"), tmp.path().join("two"));
        std::fs::create_dir_all(&config).unwrap();
        let old = Remote {
            server: Some("127.0.0.1:7500".into()),
            repo: "shop".into(),
            token: Some("kito_ada".into()),
            fingerprint: Some("sha256:00".into()),
            ..Remote::default()
        };
        old.save(&one).unwrap();
        Remote { repo: "notes".into(), ..old.clone() }.save(&two).unwrap();
        // As it was, it works as it did, with what it keeps.
        let as_it_was = Remote::read(&one).unwrap();
        assert!(!as_it_was.names_its_server());
        assert_eq!(as_it_was.filled(&Servers::default()), old);

        let named = |name: &str| Some(Adopted { name: name.to_owned(), beside: None });
        assert_eq!(adopt_in(&one, &config).unwrap(), named("127.0.0.1:7500"));
        let kept = std::fs::read_to_string(one.join(".kito").join(REMOTE_FILE)).unwrap();
        assert_eq!(kept, "server = \"127.0.0.1:7500\"\nrepo = \"shop\"\n");
        // The second directory of the same connection uses the entry the first made.
        assert_eq!(adopt_in(&two, &config).unwrap(), named("127.0.0.1:7500"));
        assert_eq!(adopt_in(&two, &config).unwrap(), None, "moved once");
        let servers = Servers::read_in(&config).unwrap();
        assert_eq!(servers.servers.len(), 1);
        assert_eq!(servers.current, None, "nothing becomes current by a move");
        // Another account of the same server, or the same one known another
        // way, is kept beside it: a directory never changes who it acts as.
        let (three, four) = (tmp.path().join("three"), tmp.path().join("four"));
        Remote { token: Some("kito_bob".into()), ..old.clone() }.save(&three).unwrap();
        Remote { fingerprint: Some("sha256:11".into()), ..old.clone() }.save(&four).unwrap();
        let beside = |name: &str| Some(Adopted { name: name.into(), beside: Some("127.0.0.1:7500".into()) });
        assert_eq!(adopt_in(&three, &config).unwrap(), beside("127.0.0.1:7500-2"));
        assert_eq!(adopt_in(&four, &config).unwrap(), beside("127.0.0.1:7500-3"));
        let servers = Servers::read_in(&config).unwrap();
        assert_eq!(servers.servers["127.0.0.1:7500-2"].token.as_deref(), Some("kito_bob"));
        assert_eq!(servers.servers["127.0.0.1:7500"].token.as_deref(), Some("kito_ada"));
        // Read, each is the whole connection it was.
        let filled = Remote::read(&two).unwrap().filled(&servers);
        assert_eq!(
            filled,
            Remote { repo: "notes".into(), name: Some("127.0.0.1:7500".into()), ..old.clone() }
        );
        // A name that is not among the person's servers is reached nowhere.
        let unknown = Remote { server: Some("elsewhere".into()), repo: "shop".into(), ..Remote::default() };
        let filled = unknown.filled(&Servers::default());
        assert_eq!((filled.server, filled.name.as_deref()), (None, Some("elsewhere")));
    }
}
