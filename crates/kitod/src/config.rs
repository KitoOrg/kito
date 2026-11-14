//! Repository configuration: `.kito/config.toml`.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CONFIG_FILE: &str = "config.toml";

/// Configuration written by `kito init`.
pub const DEFAULT_CONFIG: &str = r#"# Kito configuration for this repository.

[author]
# Who signs the changes captured in the main working directory.
# Set it with `kito identity --name ... --email ...`. If it is missing, these
# apply, in order: $KITO_AUTHOR and $KITO_EMAIL, the personal configuration
# (~/.config/kito/config.toml), the git identity, $USER.
# name = "Ada"
# email = "ada@example.com"
# kind = "human"        # or "agent"

[capture]
# How long to wait for quiet after the last watcher event, in milliseconds.
debounce_ms = 50
# How often, in seconds, a full scan reconciles the missed events.
# If the daemon notices that events are not arriving (for example on /mnt/c
# under WSL, or on a network directory) it scans much more often by itself.
rescan_secs = 60
# Use the operating system's events. With `false` the daemon relies only on
# frequent scanning.
watch = true
# Paths to ignore, in .gitignore syntax. They are added to .gitignore,
# .git/info/exclude and .kitoignore. `.git` and `.kito` are always ignored.
ignore = []
# Files larger than this are not captured.
max_file_bytes = 268435456

[git]
# Export every checkpoint as a git commit as soon as it is created.
export_on_checkpoint = true
# Branch to advance. If it is missing: the branch HEAD points to.
# branch = "main"
# Domain of the email addresses of the exported commits.
email_domain = "kito.invalid"

[lanes]
# Untracked paths of the main working directory that every lane sees
# through a link instead of a copy: what the tools need and no lane changes.
# share = ["node_modules", ".venv"]

[verify]
# Commands that must succeed before a lane is integrated into the main lane.
# They run in order, with `sh -c`, in a copy of the merged result.
# commands = ["npx tsc --noEmit", "npm test"]
# Seconds after which a command is stopped and counts as failed.
timeout_secs = 600
"#;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub author: AuthorConfig,
    pub capture: CaptureConfig,
    pub git: GitConfig,
    pub lanes: LanesConfig,
    pub verify: VerifyConfig,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LanesConfig {
    /// Untracked paths of the main working directory that every lane sees
    /// through a link instead of a copy (`node_modules`, `.venv`): what the
    /// tools need and no lane should change.
    pub share: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyConfig {
    /// Commands that must succeed, in order, before a lane is integrated.
    /// They run with `sh -c` in a copy of the merged result.
    pub commands: Vec<String>,
    /// Time after which a command is stopped and counts as failed.
    pub timeout_secs: u64,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        VerifyConfig { commands: Vec::new(), timeout_secs: 600 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthorConfig {
    pub name: Option<String>,
    pub email: Option<String>,
    pub kind: Option<AuthorKindConfig>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthorKindConfig {
    #[default]
    Human,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub debounce_ms: u64,
    pub rescan_secs: u64,
    pub watch: bool,
    pub ignore: Vec<String>,
    pub max_file_bytes: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            debounce_ms: 50,
            rescan_secs: 60,
            watch: true,
            ignore: Vec::new(),
            max_file_bytes: 256 << 20,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    pub export_on_checkpoint: bool,
    pub branch: Option<String>,
    pub email_domain: String,
}

impl Default for GitConfig {
    fn default() -> Self {
        GitConfig { export_on_checkpoint: true, branch: None, email_domain: "kito.invalid".into() }
    }
}

impl Config {
    /// Reads `config.toml` from the store directory. If the file is missing
    /// the defaults apply.
    pub fn load(store_dir: &Path) -> Result<Config, String> {
        let path = store_dir.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn export_options(&self) -> kito_git::ExportOptions {
        kito_git::ExportOptions {
            branch: self.git.branch.clone(),
            email_domain: self.git.email_domain.clone(),
            update_index: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_file_parses_to_the_defaults() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn partial_files_and_unknown_keys_are_accepted() {
        let parsed: Config = toml::from_str(
            "[author]\nname = \"ada\"\nkind = \"agent\"\n[capture]\ndebounce_ms = 10\n[future]\nx = 1\n",
        )
        .unwrap();
        assert_eq!(parsed.author.name.as_deref(), Some("ada"));
        assert_eq!(parsed.author.kind, Some(AuthorKindConfig::Agent));
        assert_eq!(parsed.capture.debounce_ms, 10);
        assert_eq!(parsed.capture.rescan_secs, 60);
    }
}
