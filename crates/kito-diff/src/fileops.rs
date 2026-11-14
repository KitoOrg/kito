//! From the differences between a tree and a new state of the files to the
//! `File*` and `TextEdit` ops.

use crate::text;
use kito_core::{BlobId, FileMode, OpKind, TextEdit, Tree};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Above this total size the text of the edits is not written into the op:
/// it can still be derived from the two blobs.
pub const MAX_INLINE_EDIT_BYTES: usize = 32 << 10;

/// New state of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub blob: BlobId,
    pub mode: FileMode,
}

/// Ops that bring `tree` to the new state described by `changes`
/// (path → new state, `None` = the file is gone).
///
/// `content` returns the content of a blob (old or new): it is needed to
/// compute the edits. Paths whose state already matches the tree produce
/// nothing. The ops are in an order that can be applied to `tree`: renames,
/// removals, additions, edits.
///
/// Rename: a file that disappeared and one that appeared with the same
/// (non-empty) content and the same type become a `FileRename`. No similarity
/// heuristics: zero false positives.
pub fn ops_for_changes(
    tree: &Tree,
    changes: &BTreeMap<PathBuf, Option<Snapshot>>,
    content: &mut dyn FnMut(&BlobId) -> Option<Vec<u8>>,
) -> Vec<OpKind> {
    let empty = BlobId::of(b"");
    let mut removed: Vec<(&PathBuf, Snapshot)> = Vec::new();
    let mut added: Vec<(&PathBuf, Snapshot)> = Vec::new();
    let mut edits = Vec::new();
    let mut retyped = Vec::new();

    for (path, new) in changes {
        let old = tree.get(path).map(|e| Snapshot { blob: e.blob, mode: e.mode });
        match (old, *new) {
            (None, None) => {}
            (Some(old), None) => removed.push((path, old)),
            (None, Some(new)) => added.push((path, new)),
            (Some(old), Some(new)) if old == new => {}
            (Some(old), Some(new)) if old.mode == new.mode => edits.push((path, old.blob, new.blob)),
            // Type change (for example chmod +x): the model has no dedicated
            // op, so it is a removal followed by an addition.
            (Some(_), Some(new)) => retyped.push((path, new)),
        }
    }

    let mut ops = Vec::new();

    // Exact renames: files that disappeared and files that appeared with the
    // same content are paired in path order.
    let mut by_content: BTreeMap<(BlobId, FileMode), Vec<&PathBuf>> = BTreeMap::new();
    for (path, snap) in &removed {
        if snap.blob != empty {
            by_content.entry((snap.blob, snap.mode)).or_default().push(path);
        }
    }
    let mut renamed_from: Vec<&PathBuf> = Vec::new();
    let mut renamed_to: Vec<&PathBuf> = Vec::new();
    for (path, snap) in &added {
        if let Some(sources) = by_content.get_mut(&(snap.blob, snap.mode)) {
            if !sources.is_empty() {
                let from = sources.remove(0);
                ops.push(OpKind::FileRename { from: from.clone(), to: (*path).clone() });
                renamed_from.push(from);
                renamed_to.push(path);
            }
        }
    }

    for (path, _) in &removed {
        if !renamed_from.contains(path) {
            ops.push(OpKind::FileRemove { path: (*path).clone() });
        }
    }
    for (path, _) in &retyped {
        ops.push(OpKind::FileRemove { path: (*path).clone() });
    }
    for (path, snap) in added.iter().chain(&retyped) {
        if !renamed_to.contains(path) {
            ops.push(OpKind::FileAdd { path: (*path).clone(), blob: snap.blob, mode: snap.mode });
        }
    }
    for (path, before, after) in edits {
        let edits = match (content(&before), content(&after)) {
            (Some(old), Some(new)) => inline_edits(&old, &new),
            _ => Vec::new(),
        };
        ops.push(OpKind::TextEdit { file: path.clone(), before, after, edits });
    }
    ops
}

/// Edits to write into the op: the exact ones if the file is text and the
/// change is small, otherwise none (the change stays described by the two blobs).
pub fn inline_edits(before: &[u8], after: &[u8]) -> Vec<TextEdit> {
    if text::is_binary(before) || text::is_binary(after) {
        return Vec::new();
    }
    let edits = text::diff(before, after);
    if edits.iter().map(|e| e.text.len()).sum::<usize>() > MAX_INLINE_EDIT_BYTES {
        return Vec::new();
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use kito_core::apply_edits;
    use proptest::prelude::*;
    use std::collections::HashMap;

    struct World {
        blobs: HashMap<BlobId, Vec<u8>>,
    }

    impl World {
        fn snap(&mut self, content: &[u8], mode: FileMode) -> Snapshot {
            let blob = BlobId::of(content);
            self.blobs.insert(blob, content.to_vec());
            Snapshot { blob, mode }
        }
    }

    fn run(tree: &Tree, changes: &BTreeMap<PathBuf, Option<Snapshot>>, world: &World) -> Vec<OpKind> {
        ops_for_changes(tree, changes, &mut |b| world.blobs.get(b).cloned())
    }

    #[test]
    fn produces_each_kind_of_op() {
        let mut w = World { blobs: HashMap::new() };
        let mut tree = Tree::new();
        for (path, content) in
            [("keep.txt", "k\n"), ("edit.txt", "a\nb\n"), ("old.txt", "content\n"), ("gone.txt", "g\n")]
        {
            let s = w.snap(content.as_bytes(), FileMode::Regular);
            tree.apply(&OpKind::FileAdd { path: path.into(), blob: s.blob, mode: s.mode }).unwrap();
        }
        let mut changes = BTreeMap::new();
        changes.insert("keep.txt".into(), Some(w.snap(b"k\n", FileMode::Regular)));
        changes.insert("edit.txt".into(), Some(w.snap(b"a\nB\n", FileMode::Regular)));
        changes.insert("old.txt".into(), None);
        changes.insert("new.txt".into(), Some(w.snap(b"content\n", FileMode::Regular)));
        changes.insert("gone.txt".into(), None);
        changes.insert("added.txt".into(), Some(w.snap(b"x\n", FileMode::Regular)));
        changes.insert("never.txt".into(), None);

        let ops = run(&tree, &changes, &w);
        let names: Vec<_> = ops.iter().map(|o| o.name()).collect();
        assert_eq!(names, ["FileRename", "FileRemove", "FileAdd", "TextEdit"]);
        assert_eq!(ops[0], OpKind::FileRename { from: "old.txt".into(), to: "new.txt".into() });
        match &ops[3] {
            OpKind::TextEdit { file, edits, .. } => {
                assert_eq!(file, &PathBuf::from("edit.txt"));
                assert_eq!(edits, &vec![TextEdit::new(2, 4, "B\n")]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_files_are_never_renames_and_mode_change_is_remove_plus_add() {
        let mut w = World { blobs: HashMap::new() };
        let mut tree = Tree::new();
        let e = w.snap(b"", FileMode::Regular);
        tree.apply(&OpKind::FileAdd { path: "a/__init__.py".into(), blob: e.blob, mode: e.mode }).unwrap();
        let s = w.snap(b"#!/bin/sh\n", FileMode::Regular);
        tree.apply(&OpKind::FileAdd { path: "run.sh".into(), blob: s.blob, mode: s.mode }).unwrap();

        let mut changes = BTreeMap::new();
        changes.insert("a/__init__.py".into(), None);
        changes.insert("b/__init__.py".into(), Some(e));
        changes.insert("run.sh".into(), Some(w.snap(b"#!/bin/sh\n", FileMode::Executable)));
        let ops = run(&tree, &changes, &w);
        let names: Vec<_> = ops.iter().map(|o| o.name()).collect();
        assert_eq!(names, ["FileRemove", "FileRemove", "FileAdd", "FileAdd"]);
    }

    #[test]
    fn big_or_binary_changes_carry_no_inline_edits() {
        assert!(inline_edits(b"a\0b", b"a\0c").is_empty());
        let big = "x\n".repeat(MAX_INLINE_EDIT_BYTES);
        assert!(inline_edits(b"y\n", big.as_bytes()).is_empty());
        assert!(!inline_edits(b"y\n", b"z\n").is_empty());
    }

    proptest! {
        /// Applying the ops produced brings the tree exactly to the new state.
        #[test]
        fn ops_bring_the_tree_to_the_new_state(
            initial in prop::collection::btree_map("[a-d]{1,2}", (0u8..6, any::<bool>()), 0..8),
            target in prop::collection::btree_map("[a-d]{1,2}", prop::option::of((0u8..6, any::<bool>())), 0..10),
        ) {
            let mut w = World { blobs: HashMap::new() };
            let content = |n: u8| if n == 0 { Vec::new() } else { format!("content {n}\nline\n").into_bytes() };
            let mode = |x: bool| if x { FileMode::Executable } else { FileMode::Regular };

            let mut tree = Tree::new();
            for (path, (n, x)) in &initial {
                let s = w.snap(&content(*n), mode(*x));
                tree.apply(&OpKind::FileAdd { path: path.into(), blob: s.blob, mode: s.mode }).unwrap();
            }
            let changes: BTreeMap<PathBuf, Option<Snapshot>> = target
                .iter()
                .map(|(p, t)| (PathBuf::from(p), t.map(|(n, x)| w.snap(&content(n), mode(x)))))
                .collect();

            let mut expected = tree.clone();
            let ops = run(&tree, &changes, &w);
            let mut result = tree.clone();
            for op in &ops {
                result.apply(op).unwrap();
                if let OpKind::TextEdit { before, after, edits, .. } = op {
                    if !edits.is_empty() {
                        prop_assert_eq!(&apply_edits(&w.blobs[before], edits).unwrap(), &w.blobs[after]);
                    }
                }
            }
            // Expected state: the initial tree with the changes written over it.
            let mut files: BTreeMap<PathBuf, Snapshot> =
                expected.iter().map(|(p, e)| (p.to_owned(), Snapshot { blob: e.blob, mode: e.mode })).collect();
            for (p, s) in &changes {
                match s {
                    Some(s) => { files.insert(p.clone(), *s); }
                    None => { files.remove(p); }
                }
            }
            let got: BTreeMap<PathBuf, Snapshot> =
                result.iter().map(|(p, e)| (p.to_owned(), Snapshot { blob: e.blob, mode: e.mode })).collect();
            prop_assert_eq!(got, files);
            // No useless ops: repeating the computation on the result produces nothing.
            prop_assert!(run(&result, &changes, &w).is_empty());
            expected = result;
            let _ = expected;
        }
    }
}
