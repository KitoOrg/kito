//! Stato di un albero di file: percorso → blob.
//!
//! Si ottiene riapplicando in ordine le operazioni di una lane. Dipende solo
//! da ciò che le op dichiarano, mai da un parser: la stessa storia dà sempre
//! lo stesso albero, anche con versioni future di Kito.

use crate::ids::BlobId;
use crate::model::{FileMode, OpKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TreeEntry {
    pub blob: BlobId,
    #[serde(default, skip_serializing_if = "FileMode::is_regular")]
    pub mode: FileMode,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("{}: il file esiste già", .0.display())]
    Exists(PathBuf),
    #[error("{}: il file non esiste", .0.display())]
    Missing(PathBuf),
    #[error("{}: contenuto {found} diverso da quello atteso dall'operazione ({expected})", path.display())]
    Mismatch { path: PathBuf, expected: BlobId, found: BlobId },
}

/// Albero di file a un certo punto della storia.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    files: BTreeMap<PathBuf, TreeEntry>,
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

    /// File in ordine di percorso.
    pub fn iter(&self) -> impl Iterator<Item = (&Path, &TreeEntry)> {
        self.files.iter().map(|(p, e)| (p.as_path(), e))
    }

    /// Applica un'operazione. Le op che non descrivono un effetto sui file
    /// (quelle simboliche) lasciano l'albero invariato. Se l'operazione non
    /// è applicabile a questo albero restituisce un errore e non cambia nulla.
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
        // Aggiungere `mode` non ha cambiato la codifica dei FileAdd esistenti.
        let v = serde_json::to_value(add("a", "1")).unwrap();
        assert!(v["FileAdd"].get("mode").is_none());
        let exec = OpKind::FileAdd { path: "a".into(), blob: blob("1"), mode: FileMode::Executable };
        assert_eq!(serde_json::to_value(&exec).unwrap()["FileAdd"]["mode"], "Executable");
    }
}
