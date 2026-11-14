//! Text differ: from two versions of a file to the list of [`TextEdit`]s.

use kito_core::TextEdit;
use similar::{Algorithm, DiffTag};
use std::time::{Duration, Instant};

/// Past this time the line diff settles for a non-minimal result.
const DEADLINE: Duration = Duration::from_millis(200);
/// Leading bytes searched for a NUL to recognize a binary file.
const BINARY_PROBE: usize = 8000;

/// `true` if the content looks binary (same heuristic as git).
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_PROBE)].contains(&0)
}

/// Edits that turn `before` into `after`: `apply_edits(before, diff) == after`.
///
/// For text the diff is line-based (Myers): each block of changed lines
/// becomes one edit. For binary files it is a single edit on the part that
/// changes, with the common prefix and suffix removed.
pub fn diff(before: &[u8], after: &[u8]) -> Vec<TextEdit> {
    if before == after {
        return Vec::new();
    }
    if is_binary(before) || is_binary(after) {
        return vec![trim_diff(before, after)];
    }
    let old = lines(before);
    let new = lines(after);
    let ops =
        similar::capture_diff_slices_deadline(Algorithm::Myers, &old, &new, Some(Instant::now() + DEADLINE));

    // Byte offset of the start of each line, plus the end.
    let offsets = |ls: &[&[u8]]| -> Vec<u32> {
        let mut at = 0u32;
        let mut out = Vec::with_capacity(ls.len() + 1);
        out.push(0);
        for l in ls {
            at += l.len() as u32;
            out.push(at);
        }
        out
    };
    let (old_at, new_at) = (offsets(&old), offsets(&new));

    let mut edits: Vec<TextEdit> = Vec::new();
    for op in ops {
        let (tag, o, n) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        let (start, end) = (old_at[o.start], old_at[o.end]);
        let text = &after[new_at[n.start] as usize..new_at[n.end] as usize];
        match edits.last_mut() {
            // Adjacent blocks are merged into a single edit.
            Some(last) if last.end == start => {
                last.end = end;
                last.text.extend_from_slice(text);
            }
            _ => edits.push(TextEdit::new(start, end, text)),
        }
    }
    edits
}

/// A single edit: the middle part that changes, with the common prefix and suffix removed.
fn trim_diff(before: &[u8], after: &[u8]) -> TextEdit {
    let prefix = before.iter().zip(after).take_while(|(a, b)| a == b).count();
    let max_suffix = before.len().min(after.len()) - prefix;
    let suffix =
        before.iter().rev().zip(after.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    TextEdit::new(prefix as u32, (before.len() - suffix) as u32, &after[prefix..after.len() - suffix])
}

/// Lines with the terminator included: concatenated, they give back the text.
fn lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|&b| b == b'\n').collect()
}

/// Statistics of a diff, for the output of `kito log`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub lines_added: usize,
    pub lines_removed: usize,
}

/// Lines added and removed by the edits, relative to the original text.
pub fn stat(before: &[u8], edits: &[TextEdit]) -> Stat {
    let count = |text: &[u8]| lines(text).len();
    let mut s = Stat::default();
    for e in edits {
        s.lines_added += count(&e.text);
        s.lines_removed += before.get(e.start as usize..e.end as usize).map_or(0, count);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use kito_core::apply_edits;
    use proptest::prelude::*;

    #[test]
    fn line_changes_become_line_ranged_edits() {
        let before = b"one\ntwo\nten\nhundred\n";
        let after = b"one\nTWO\nten\nhundred\nthousand\n";
        let edits = diff(before, after);
        assert_eq!(edits, vec![TextEdit::new(4, 8, "TWO\n"), TextEdit::new(20, 20, "thousand\n")]);
        assert_eq!(apply_edits(before, &edits).unwrap(), after);
        assert_eq!(stat(before, &edits), Stat { lines_added: 2, lines_removed: 1 });
    }

    #[test]
    fn identical_inputs_have_no_edits() {
        assert!(diff(b"abc\n", b"abc\n").is_empty());
        assert!(diff(b"", b"").is_empty());
    }

    #[test]
    fn missing_final_newline_is_handled() {
        for (a, b) in
            [(&b"a\nb"[..], &b"a\nb\n"[..]), (b"a\nb\n", b"a\nb"), (b"", b"x"), (b"x", b""), (b"a", b"b")]
        {
            assert_eq!(apply_edits(a, &diff(a, b)).unwrap(), b, "{a:?} → {b:?}");
        }
    }

    #[test]
    fn binary_files_get_one_trimmed_edit() {
        let before = [1u8, 2, 0, 3, 4, 5, 6];
        let after = [1u8, 2, 0, 9, 9, 9, 5, 6];
        let edits = diff(&before, &after);
        assert_eq!(edits, vec![TextEdit::new(3, 5, vec![9, 9, 9])]);
        assert_eq!(apply_edits(&before, &edits).unwrap(), after);
    }

    proptest! {
        #[test]
        fn diff_then_apply_is_identity_on_text(
            a in prop::collection::vec("[abc ]{0,6}\n?", 0..40),
            b in prop::collection::vec("[abc ]{0,6}\n?", 0..40),
        ) {
            let (a, b) = (a.concat().into_bytes(), b.concat().into_bytes());
            let edits = diff(&a, &b);
            prop_assert_eq!(apply_edits(&a, &edits).unwrap(), b);
            // The edits are sorted and non-adjacent (adjacent ones are merged).
            for pair in edits.windows(2) {
                prop_assert!(pair[0].end < pair[1].start);
            }
        }

        #[test]
        fn diff_then_apply_is_identity_on_bytes(
            a in prop::collection::vec(any::<u8>(), 0..200),
            b in prop::collection::vec(any::<u8>(), 0..200),
        ) {
            prop_assert_eq!(apply_edits(&a, &diff(&a, &b)).unwrap(), b);
        }

        #[test]
        fn small_change_in_big_file_gives_small_edit(n in 50usize..400, at in any::<prop::sample::Index>()) {
            let before: Vec<String> = (0..n).map(|i| format!("line number {i}\n")).collect();
            let mut after = before.clone();
            let i = at.index(n);
            after[i] = "changed\n".to_string();
            let edits = diff(before.concat().as_bytes(), after.concat().as_bytes());
            prop_assert_eq!(edits.len(), 1);
            prop_assert_eq!(&edits[0].text, b"changed\n");
        }
    }
}
