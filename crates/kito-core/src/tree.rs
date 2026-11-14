//! State of a file tree: path → blob.
//!
//! It is obtained by replaying the operations of a lane in order. It depends
//! only on what the ops declare, never on a parser: the same history always
//! gives the same tree, even with future versions of Kito.

use crate::ids::BlobId;
use crate::model::{FileMode, OpKind};
use imbl::ordmap::DiffItem;
use imbl::OrdMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TreeEntry {
    pub blob: BlobId,
    #[serde(default, skip_serializing_if = "FileMode::is_regular")]
    pub mode: FileMode,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("{}: the file already exists", .0.display())]
    Exists(PathBuf),
    #[error("{}: the file does not exist", .0.display())]
    Missing(PathBuf),
    #[error("{}: content {found} differs from the one the operation expects ({expected})", path.display())]
    Mismatch { path: PathBuf, expected: BlobId, found: BlobId },
}

/// File tree at a given point in the history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    // A persistent map: a copy shares everything with the original and
    // costs nothing, whatever the number of files. A lane starts from a
    // copy of the main tree, and gets a new one at every integration.
    files: OrdMap<PathBuf, TreeEntry>,
}

impl Tree {
    pub fn new() -> Tree {
        Tree::default()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn get(&self, path: &Path) -> Option<&TreeEntry> {
        self.files.get(path)
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }

    /// Files in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&Path, &TreeEntry)> {
        self.files.iter().map(|(p, e)| (p.as_path(), e))
    }

    /// The paths at which this tree and `other` differ: a file with another
    /// content or type, or one that only one of the two has. In path order.
    /// What two trees share because one was copied from the other is not
    /// even looked at: the cost follows the differences, not the files.
    pub fn differences<'a>(&'a self, other: &'a Tree) -> impl Iterator<Item = &'a Path> {
        self.files.diff(&other.files).map(|item| match item {
            DiffItem::Add(path, _) | DiffItem::Remove(path, _) => path.as_path(),
            DiffItem::Update { old: (path, _), .. } => path.as_path(),
        })
    }

    /// Gives a file another content, keeping its type.
    pub fn replace(&mut self, path: &Path, blob: BlobId) -> Result<(), TreeError> {
        let entry = self.files.get_mut(path).ok_or_else(|| TreeError::Missing(path.to_owned()))?;
        entry.blob = blob;
        Ok(())
    }

    /// Files contained in the directory `dir` (at any depth).
    pub fn under<'a>(&'a self, dir: &'a Path) -> impl Iterator<Item = (&'a Path, &'a TreeEntry)> {
        // Paths are ordered by components: the descendants of `dir` are
        // contiguous and come right after `dir`.
        self.files
            .range::<_, Path>((std::ops::Bound::Excluded(dir), std::ops::Bound::Unbounded))
            .take_while(move |(p, _)| p.starts_with(dir))
            .map(|(p, e)| (p.as_path(), e))
    }

    /// Applies an operation. Ops that do not describe an effect on files
    /// (the symbolic ones) leave the tree unchanged. If the operation cannot
    /// be applied to this tree, it returns an error and changes nothing.
    pub fn apply(&mut self, kind: &OpKind) -> Result<(), TreeError> {
        match kind {
            OpKind::FileAdd { path, blob, mode } => {
                if self.files.contains_key(path) {
                    return Err(TreeError::Exists(path.clone()));
                }
                self.files.insert(path.clone(), TreeEntry { blob: *blob, mode: *mode });
            }
            OpKind::FileRemove { path } => {
                self.files.remove(path).ok_or_else(|| TreeError::Missing(path.clone()))?;
            }
            OpKind::FileRename { from, to } => {
                if self.files.contains_key(to) {
                    return Err(TreeError::Exists(to.clone()));
                }
                let entry = self.files.remove(from).ok_or_else(|| TreeError::Missing(from.clone()))?;
                self.files.insert(to.clone(), entry);
            }
            OpKind::TextEdit { file, before, after, .. } => {
                let entry = self.files.get_mut(file).ok_or_else(|| TreeError::Missing(file.clone()))?;
                if entry.blob != *before {
                    return Err(TreeError::Mismatch {
                        path: file.clone(),
                        expected: *before,
                        found: entry.blob,
                    });
                }
                entry.blob = *after;
            }
            OpKind::AddSymbol { .. }
            | OpKind::RemoveSymbol { .. }
            | OpKind::ModifyBody { .. }
            | OpKind::ModifySignature { .. }
            | OpKind::Rename { .. }
            | OpKind::Move { .. } => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(s: &str) -> BlobId {
        BlobId::of(s.as_bytes())
    }

    fn add(path: &str, content: &str) -> OpKind {
        OpKind::FileAdd { path: path.into(), blob: blob(content), mode: FileMode::Regular }
    }

    #[test]
    fn differences_between_two_trees() {
        let tree = |files: &[(&str, &str)]| {
            let mut tree = Tree::new();
            files.iter().for_each(|(path, content)| tree.apply(&add(path, content)).unwrap());
            tree
        };
        let one = tree(&[("a.txt", "1"), ("b/c.txt", "2"), ("b/d.txt", "3"), ("z.txt", "4")]);
        let two = tree(&[("a.txt", "1"), ("b/c.txt", "changed"), ("b/e.txt", "5"), ("z.txt", "4")]);
        let paths =
            |a: &Tree, b: &Tree| a.differences(b).map(|p| p.display().to_string()).collect::<Vec<_>>();
        assert_eq!(paths(&one, &two), ["b/c.txt", "b/d.txt", "b/e.txt"]);
        assert_eq!(paths(&two, &one), ["b/c.txt", "b/d.txt", "b/e.txt"]);
        assert!(paths(&one, &one).is_empty());
        assert_eq!(paths(&one, &Tree::new()).len(), 4);
        assert_eq!(paths(&Tree::new(), &two).len(), 4);
    }

    #[test]
    fn replaying_ops_gives_the_tree() {
        let mut t = Tree::new();
        t.apply(&add("a.txt", "1")).unwrap();
        t.apply(&OpKind::FileAdd { path: "run.sh".into(), blob: blob("#!"), mode: FileMode::Executable })
            .unwrap();
        t.apply(&OpKind::TextEdit {
            file: "a.txt".into(),
            before: blob("1"),
            after: blob("2"),
            edits: vec![],
        })
        .unwrap();
        t.apply(&OpKind::FileRename { from: "a.txt".into(), to: "dir/b.txt".into() }).unwrap();
        t.apply(&OpKind::FileRemove { path: "run.sh".into() }).unwrap();
        let files: Vec<_> = t.iter().collect();
        assert_eq!(
            files,
            [(Path::new("dir/b.txt"), &TreeEntry { blob: blob("2"), mode: FileMode::Regular })]
        );
    }

    #[test]
    fn under_lists_exactly_the_descendants() {
        let mut t = Tree::new();
        for p in ["a", "a.txt", "a/b", "a/c/d", "ab/x", "b"] {
            if p != "a" {
                t.apply(&add(p, p)).unwrap();
            }
        }
        let under: Vec<_> = t.under(Path::new("a")).map(|(p, _)| p.to_str().unwrap()).collect();
        assert_eq!(under, ["a/b", "a/c/d"]);
        assert_eq!(t.under(Path::new("zz")).count(), 0);
        assert_eq!(t.under(Path::new("a/c")).count(), 1);
    }

    #[test]
    fn inapplicable_ops_are_rejected_without_side_effects() {
        let mut t = Tree::new();
        t.apply(&add("a", "1")).unwrap();
        t.apply(&add("b", "2")).unwrap();
        let before = t.clone();
        assert_eq!(t.apply(&add("a", "x")), Err(TreeError::Exists("a".into())));
        assert_eq!(t.apply(&OpKind::FileRemove { path: "z".into() }), Err(TreeError::Missing("z".into())));
        assert_eq!(
            t.apply(&OpKind::FileRename { from: "a".into(), to: "b".into() }),
            Err(TreeError::Exists("b".into()))
        );
        assert_eq!(
            t.apply(&OpKind::FileRename { from: "z".into(), to: "c".into() }),
            Err(TreeError::Missing("z".into()))
        );
        assert!(matches!(
            t.apply(&OpKind::TextEdit {
                file: "a".into(),
                before: blob("9"),
                after: blob("3"),
                edits: vec![]
            }),
            Err(TreeError::Mismatch { .. })
        ));
        assert_eq!(t, before);
    }

    #[test]
    fn regular_mode_is_not_encoded() {
        // Adding `mode` did not change the encoding of existing FileAdd ops.
        let v = serde_json::to_value(add("a", "1")).unwrap();
        assert!(v["FileAdd"].get("mode").is_none());
        let exec = OpKind::FileAdd { path: "a".into(), blob: blob("1"), mode: FileMode::Executable };
        assert_eq!(serde_json::to_value(&exec).unwrap()["FileAdd"]["mode"], "Executable");
    }
}
