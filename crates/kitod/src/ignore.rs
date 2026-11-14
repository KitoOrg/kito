//! Quali percorsi non vengono catturati.
//!
//! Stesse regole per la scansione e per gli eventi del watcher, altrimenti
//! un file apparirebbe e sparirebbe a seconda di chi lo guarda. `.git` e
//! `.kito` sono sempre ignorati; poi valgono `.gitignore` (anche annidati),
//! `.git/info/exclude`, `.kitoignore` e i pattern della configurazione.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct IgnoreRules {
    root: PathBuf,
    /// Pattern validi per tutto il repository (configurazione, `.kitoignore`,
    /// `.git/info/exclude`).
    global: Gitignore,
    /// `.gitignore` di ogni cartella, letti quando servono.
    per_dir: Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
}

impl IgnoreRules {
    pub fn new(root: &Path, patterns: &[String]) -> IgnoreRules {
        let mut builder = GitignoreBuilder::new(root);
        for pattern in patterns {
            // Un pattern malformato nella configurazione non deve fermare il daemon.
            let _ = builder.add_line(None, pattern);
        }
        for file in [root.join(".kitoignore"), root.join(".git/info/exclude")] {
            if file.is_file() {
                let _ = builder.add(file);
            }
        }
        let global = builder.build().unwrap_or_else(|_| Gitignore::empty());
        IgnoreRules { root: root.to_owned(), global, per_dir: Mutex::new(HashMap::new()) }
    }

    /// Dimentica i `.gitignore` letti (uno di essi è cambiato).
    pub fn invalidate(&self) {
        self.per_dir.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn gitignore_of(&self, dir: &Path) -> Option<Arc<Gitignore>> {
        let mut cache = self.per_dir.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.get(dir) {
            return cached.clone();
        }
        let abs = self.root.join(dir);
        let file = abs.join(".gitignore");
        let loaded = file.is_file().then(|| {
            let mut builder = GitignoreBuilder::new(&abs);
            let _ = builder.add(&file);
            Arc::new(builder.build().unwrap_or_else(|_| Gitignore::empty()))
        });
        cache.insert(dir.to_owned(), loaded.clone());
        loaded
    }

    /// `true` se il percorso (relativo alla radice) è ignorato. Come in git,
    /// se una cartella è ignorata lo è tutto ciò che contiene, e un
    /// `.gitignore` più profondo prevale su uno più in alto.
    pub fn is_ignored(&self, rel: &Path, is_dir: bool) -> bool {
        let parts: Vec<&std::ffi::OsStr> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        if parts.is_empty() {
            return false;
        }
        if parts.contains(&".git".as_ref()) || parts[0] == ".kito" {
            return true;
        }
        let mut prefix = PathBuf::new();
        for (i, part) in parts.iter().enumerate() {
            let parent = prefix.clone();
            prefix.push(part);
            let prefix_is_dir = is_dir || i + 1 < parts.len();
            if self.prefix_ignored(&prefix, &parent, prefix_is_dir) {
                return true;
            }
        }
        false
    }

    /// Decide per un singolo prefisso, consultando i `.gitignore` dalla sua
    /// cartella in su: la prima regola che parla vince.
    fn prefix_ignored(&self, prefix: &Path, parent: &Path, is_dir: bool) -> bool {
        let abs = self.root.join(prefix);
        let mut dir = Some(parent);
        while let Some(d) = dir {
            if let Some(gitignore) = self.gitignore_of(d) {
                match gitignore.matched(&abs, is_dir) {
                    ignore::Match::Ignore(_) => return true,
                    ignore::Match::Whitelist(_) => return false,
                    ignore::Match::None => {}
                }
            }
            dir = if d.as_os_str().is_empty() { None } else { Some(d.parent().unwrap_or(Path::new(""))) };
        }
        self.global.matched(&abs, is_dir).is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn follows_gitignore_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src/gen")).unwrap();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n!importante.log\n/solo-radice.txt\n").unwrap();
        fs::write(root.join("src/.gitignore"), "gen/\n!*.log\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "locale.txt\n").unwrap();
        fs::write(root.join(".kitoignore"), "*.secret\n").unwrap();
        let rules = IgnoreRules::new(root, &["node_modules/".to_owned(), "[malformato".to_owned()]);

        let ignored = |p: &str, dir: bool| rules.is_ignored(Path::new(p), dir);
        // Sempre ignorati.
        assert!(ignored(".git", true) && ignored(".git/config", false) && ignored("sub/.git/HEAD", false));
        assert!(ignored(".kito/oplog/0.seg", false));
        assert!(!ignored("src/.kito", false), ".kito conta solo alla radice");
        // .gitignore alla radice.
        assert!(ignored("target", true) && ignored("target/debug/x", false));
        assert!(!ignored("target", false), "`target/` vale solo per le cartelle");
        assert!(ignored("a.log", false) && ignored("deep/dir/b.log", false));
        assert!(!ignored("importante.log", false));
        assert!(ignored("solo-radice.txt", false) && !ignored("sub/solo-radice.txt", false));
        // .gitignore annidato: prevale su quello più in alto.
        assert!(!ignored("src/x.log", false));
        assert!(ignored("src/gen", true) && ignored("src/gen/file.rs", false));
        // Altre fonti.
        assert!(ignored("locale.txt", false) && ignored("x/y.secret", false));
        assert!(ignored("node_modules", true) && ignored("pkg/node_modules/a.js", false));
        assert!(!ignored("src/main.rs", false) && !ignored("", false));
    }

    #[test]
    fn invalidate_rereads_gitignore_files() {
        let tmp = tempfile::tempdir().unwrap();
        let rules = IgnoreRules::new(tmp.path(), &[]);
        assert!(!rules.is_ignored(Path::new("a.tmp"), false));
        fs::write(tmp.path().join(".gitignore"), "*.tmp\n").unwrap();
        assert!(!rules.is_ignored(Path::new("a.tmp"), false), "ancora in cache");
        rules.invalidate();
        assert!(rules.is_ignored(Path::new("a.tmp"), false));
    }
}
