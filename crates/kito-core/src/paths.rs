//! Paths as Kito keeps them: relative to the root, `/`-separated, on every
//! platform (ADR 0002). A path read from disk on Windows has `\`; it is
//! converted where it enters, and Windows accepts `/` where it leaves.

use std::path::{Path, PathBuf};

/// `path` with `/` between its parts. On Unix it is the path as it is.
pub fn portable_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy();
        if text.contains('\\') {
            return PathBuf::from(text.replace('\\', "/"));
        }
    }
    path.to_owned()
}

/// The same, for a path given as text (by an agent, on a command line).
pub fn portable_text(text: &str) -> String {
    match cfg!(windows) {
        true => text.replace('\\', "/"),
        false => text.to_owned(),
    }
}

/// `dir/name` with `/`, `name` alone when `dir` is empty.
pub fn join_portable(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    if dir.as_os_str().is_empty() {
        return PathBuf::from(name);
    }
    match cfg!(windows) {
        true => PathBuf::from(format!("{}/{}", dir.to_string_lossy(), name.to_string_lossy())),
        false => dir.join(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_kept_with_slashes() {
        assert_eq!(join_portable(Path::new(""), "a.txt".as_ref()), PathBuf::from("a.txt"));
        assert_eq!(join_portable(Path::new("src/deep"), "a.ts".as_ref()), PathBuf::from("src/deep/a.ts"));
        assert_eq!(portable_path(Path::new("src/a.ts")), PathBuf::from("src/a.ts"));
        if cfg!(windows) {
            assert_eq!(portable_path(Path::new(r"src\deep\a.ts")), PathBuf::from("src/deep/a.ts"));
            assert_eq!(portable_text(r"src\a.ts:getUser"), "src/a.ts:getUser");
        } else {
            // On Unix a backslash is part of a name.
            assert_eq!(portable_path(Path::new(r"odd\name")), PathBuf::from(r"odd\name"));
        }
    }
}
