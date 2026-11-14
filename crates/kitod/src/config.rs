//! Configurazione del repository: `.kito/config.toml`.

use kito_core::{Author, AuthorKind};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CONFIG_FILE: &str = "config.toml";

/// Configurazione scritta da `kito init`.
pub const DEFAULT_CONFIG: &str = r#"# Configurazione di Kito per questo repository.

[author]
# Chi firma le modifiche catturate nella cartella di lavoro principale.
# Se manca si usa $KITO_AUTHOR, poi $USER.
# name = "francesco"
# kind = "human"        # oppure "agent"

[capture]
# Attesa di quiete dopo l'ultimo evento del watcher, in millisecondi.
debounce_ms = 50
# Ogni quanti secondi una scansione completa riconcilia gli eventi persi.
rescan_secs = 60
# Percorsi da ignorare, con la sintassi di .gitignore. Si aggiungono a
# .gitignore, .git/info/exclude e .kitoignore. `.git` e `.kito` sono sempre ignorati.
ignore = []
# I file più grandi di così non vengono catturati.
max_file_bytes = 268435456

[git]
# Esporta ogni checkpoint come commit git appena viene creato.
export_on_checkpoint = true
# Ramo da far avanzare. Se manca: il ramo a cui punta HEAD.
# branch = "main"
# Dominio degli indirizzi email dei commit esportati.
email_domain = "kito.invalid"
"#;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub author: AuthorConfig,
    pub capture: CaptureConfig,
    pub git: GitConfig,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthorConfig {
    pub name: Option<String>,
    pub kind: AuthorKindConfig,
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
    pub ignore: Vec<String>,
    pub max_file_bytes: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig { debounce_ms: 50, rescan_secs: 60, ignore: Vec::new(), max_file_bytes: 256 << 20 }
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
    /// Legge `config.toml` dalla cartella dello store. Se il file manca
    /// valgono i default.
    pub fn load(store_dir: &Path) -> Result<Config, String> {
        let path = store_dir.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Autore delle modifiche catturate nella cartella di lavoro principale.
    pub fn author(&self) -> Author {
        let name = self
            .author
            .name
            .clone()
            .or_else(|| std::env::var("KITO_AUTHOR").ok())
            .or_else(|| std::env::var("USER").ok())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "sconosciuto".to_owned());
        let kind = match self.author.kind {
            AuthorKindConfig::Human => AuthorKind::Human,
            AuthorKindConfig::Agent => AuthorKind::Agent,
        };
        Author { kind, name }
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
            "[author]\nname = \"ada\"\nkind = \"agent\"\n[capture]\ndebounce_ms = 10\n[futuro]\nx = 1\n",
        )
        .unwrap();
        assert_eq!(parsed.author(), Author::agent("ada"));
        assert_eq!(parsed.capture.debounce_ms, 10);
        assert_eq!(parsed.capture.rescan_secs, 60);
    }
}
