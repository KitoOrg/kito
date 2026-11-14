//! Who signs the changes: name, email and kind (human or agent).
//!
//! The identity is looked up, in order, in: the repository configuration,
//! the environment variables, the personal configuration, the git identity,
//! the system user. Name and email are each resolved on their own, as in git.

use crate::config::{AuthorConfig, AuthorKindConfig, Config, CONFIG_FILE};
use kito_core::{Author, AuthorKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the name of the identity in use comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Repository,
    Environment,
    Personal,
    Git,
    SystemUser,
    Unknown,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Repository => "repository configuration (.kito/config.toml)",
            Source::Environment => "environment variables (KITO_AUTHOR, KITO_EMAIL)",
            Source::Personal => "personal configuration (~/.config/kito/config.toml)",
            Source::Git => "git identity (user.name, user.email)",
            Source::SystemUser => "system user ($USER)",
            Source::Unknown => "none (it could not be determined)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub email: Option<String>,
    /// `human` or `agent`.
    pub kind: String,
    pub source: Source,
}

impl Identity {
    pub fn author(&self) -> Author {
        let kind = if self.kind == "agent" { AuthorKind::Agent } else { AuthorKind::Human };
        Author { kind, name: self.name.clone(), email: self.email.clone() }
    }
}

/// Personal configuration file: `$KITO_CONFIG_DIR/config.toml`, then
/// `$XDG_CONFIG_HOME/kito/config.toml`, then `~/.config/kito/config.toml`.
pub fn personal_config_path() -> Option<PathBuf> {
    let non_empty = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = non_empty("KITO_CONFIG_DIR") {
        return Some(dir.join(CONFIG_FILE));
    }
    let base = non_empty("XDG_CONFIG_HOME").or_else(|| non_empty("HOME").map(|home| home.join(".config")))?;
    Some(base.join("kito").join(CONFIG_FILE))
}

fn personal_author() -> AuthorConfig {
    personal_config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| toml::from_str::<Config>(&text).ok())
        .map(|config| config.author)
        .unwrap_or_default()
}

fn clean(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Identity used to sign the changes of the repository in `root`.
pub fn resolve(repo: &Config, root: &Path) -> Identity {
    let env = |name: &str| clean(std::env::var(name).ok());
    let personal = personal_author();
    let git = kito_git::git_identity(root);

    let (name, source) = [
        (clean(repo.author.name.clone()), Source::Repository),
        (env("KITO_AUTHOR"), Source::Environment),
        (clean(personal.name.clone()), Source::Personal),
        (git.as_ref().map(|(name, _)| name.clone()), Source::Git),
        (env("USER"), Source::SystemUser),
    ]
    .into_iter()
    .find_map(|(name, source)| name.map(|n| (n, source)))
    .unwrap_or_else(|| ("unknown".to_owned(), Source::Unknown));

    let email = clean(repo.author.email.clone())
        .or_else(|| env("KITO_EMAIL"))
        .or_else(|| clean(personal.email.clone()))
        .or_else(|| git.and_then(|(_, email)| email));

    let kind = match repo.author.kind.or(personal.kind).unwrap_or_default() {
        AuthorKindConfig::Human => "human",
        AuthorKindConfig::Agent => "agent",
    };
    Identity { name, email, kind: kind.to_owned(), source }
}

/// Writes the identity into the `[author]` table of a configuration file,
/// leaving everything else intact (comments included). `None` values are
/// left untouched.
pub fn write(path: &Path, name: Option<&str>, email: Option<&str>, kind: Option<&str>) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    if !doc.contains_key("author") {
        doc["author"] = toml_edit::table();
    }
    let author =
        doc["author"].as_table_mut().ok_or_else(|| format!("{}: `author` is not a table", path.display()))?;
    for (key, value) in [("name", name), ("email", email), ("kind", kind)] {
        if let Some(value) = value {
            author[key] = toml_edit::value(value);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    kito_store::fs::atomic_write(path, doc.to_string().as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CONFIG;

    #[test]
    fn writing_keeps_comments_and_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, DEFAULT_CONFIG).unwrap();
        write(&path, Some("Ada Lovelace"), Some("ada@example.com"), None).unwrap();
        write(&path, None, None, Some("agent")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Kito configuration for this repository."));
        assert!(text.contains("debounce_ms = 50"));
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.author.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(config.author.email.as_deref(), Some("ada@example.com"));
        assert_eq!(config.author.kind, Some(AuthorKindConfig::Agent));
        assert_eq!(config.capture, crate::config::CaptureConfig::default());

        // In a file that does not exist yet, too.
        let fresh = dir.path().join("new/directory/config.toml");
        write(&fresh, Some("Bo"), None, None).unwrap();
        let config: Config = toml::from_str(&std::fs::read_to_string(&fresh).unwrap()).unwrap();
        assert_eq!(config.author.name.as_deref(), Some("Bo"));
    }

    #[test]
    fn repository_settings_win_and_identity_becomes_an_author() {
        let dir = tempfile::tempdir().unwrap();
        let config: Config =
            toml::from_str("[author]\nname = \"Ada\"\nemail = \"ada@example.com\"\nkind = \"agent\"\n")
                .unwrap();
        let identity = resolve(&config, dir.path());
        assert_eq!(identity.source, Source::Repository);
        assert_eq!(identity.author(), Author::agent("Ada").with_email("ada@example.com"));
        assert_eq!(identity.author().to_string(), "Ada <ada@example.com>");
    }
}
