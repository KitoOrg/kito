//! Which paths are not captured.
//!
//! The same rules hold for the scan and for the watcher events, otherwise a
//! file would appear and disappear depending on who looks at it. `.git` and
//! `.kito` are always ignored; then come `.gitignore` (nested ones too),
//! `.git/info/exclude`, `.kitoignore` and the patterns of the configuration.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct IgnoreRules {
    root: PathBuf,
    /// Patterns valid for the whole repository (configuration, `.kitoignore`,
    /// `.git/info/exclude`).
    global: Gitignore,
    /// The `.gitignore` of each directory, read when needed.
    per_dir: Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
}

impl IgnoreRules {
    pub fn new(root: &Path, patterns: &[String]) -> IgnoreRules {
        let mut builder = GitignoreBuilder::new(root);
        for pattern in patterns {
            // A malformed pattern in the configuration must not stop the daemon.
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

    /// Forgets the `.gitignore` files read so far (one of them has changed).
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

    /// `true` if the path (relative to the root) is ignored. As in git, if a
    /// directory is ignored so is everything it contains, and a deeper
    /// `.gitignore` takes precedence over one higher up.
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

    /// Decides for a single prefix, consulting the `.gitignore` files from
    /// its directory upwards: the first rule that speaks wins.
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
        fs::write(root.join(".gitignore"), "target/\n*.log\n!important.log\n/root-only.txt\n").unwrap();
        fs::write(root.join("src/.gitignore"), "gen/\n!*.log\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "local.txt\n").unwrap();
        fs::write(root.join(".kitoignore"), "*.secret\n").unwrap();
        let rules = IgnoreRules::new(root, &["node_modules/".to_owned(), "[malformed".to_owned()]);

        let ignored = |p: &str, dir: bool| rules.is_ignored(Path::new(p), dir);
        // Always ignored.
        assert!(ignored(".git", true) && ignored(".git/config", false) && ignored("sub/.git/HEAD", false));
        assert!(ignored(".kito/oplog/0.seg", false));
        assert!(!ignored("src/.kito", false), ".kito only counts at the root");
        // .gitignore at the root.
        assert!(ignored("target", true) && ignored("target/debug/x", false));
        assert!(!ignored("target", false), "`target/` only applies to directories");
        assert!(ignored("a.log", false) && ignored("deep/dir/b.log", false));
        assert!(!ignored("important.log", false));
        assert!(ignored("root-only.txt", false) && !ignored("sub/root-only.txt", false));
        // Nested .gitignore: it takes precedence over the one higher up.
        assert!(!ignored("src/x.log", false));
        assert!(ignored("src/gen", true) && ignored("src/gen/file.rs", false));
        // Other sources.
        assert!(ignored("local.txt", false) && ignored("x/y.secret", false));
        assert!(ignored("node_modules", true) && ignored("pkg/node_modules/a.js", false));
        assert!(!ignored("src/main.rs", false) && !ignored("", false));
    }

    #[test]
    fn invalidate_rereads_gitignore_files() {
        let tmp = tempfile::tempdir().unwrap();
        let rules = IgnoreRules::new(tmp.path(), &[]);
        assert!(!rules.is_ignored(Path::new("a.tmp"), false));
        fs::write(tmp.path().join(".gitignore"), "*.tmp\n").unwrap();
        assert!(!rules.is_ignored(Path::new("a.tmp"), false), "still cached");
        rules.invalidate();
        assert!(rules.is_ignored(Path::new("a.tmp"), false));
    }
}
