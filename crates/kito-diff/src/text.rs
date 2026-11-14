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

/// A block of base lines replaced on one side: `[start, end)` in the base,
/// and the lines that take its place.
struct Hunk<'a> {
    start: usize,
    end: usize,
    lines: &'a [&'a [u8]],
}

fn hunks<'a>(base: &[&[u8]], side: &'a [&'a [u8]]) -> Vec<Hunk<'a>> {
    let ops =
        similar::capture_diff_slices_deadline(Algorithm::Myers, base, side, Some(Instant::now() + DEADLINE));
    let mut out: Vec<Hunk<'a>> = Vec::new();
    for op in ops {
        let (tag, b, s) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.end == b.start => {
                last.end = b.end;
                let from = s.end - (last.lines.len() + s.len());
                last.lines = &side[from..s.end];
            }
            _ => out.push(Hunk { start: b.start, end: b.end, lines: &side[s] }),
        }
    }
    out
}

/// Three-way merge of text: the changes `ours` and `theirs` each made to
/// `base`, both applied. `None` if they cannot be: the two sides changed the
/// same lines in different ways, or the content is binary.
///
/// Changes to different lines never conflict, even when the lines touch;
/// the same change made on both sides is applied once. The result does not
/// depend on which side is called `ours`.
pub fn merge3(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    if ours == base || ours == theirs {
        return Some(theirs.to_vec());
    }
    if theirs == base {
        return Some(ours.to_vec());
    }
    if is_binary(base) || is_binary(ours) || is_binary(theirs) {
        return None;
    }
    let (b, o, t) = (lines(base), lines(ours), lines(theirs));
    let (ours, theirs) = (hunks(&b, &o), hunks(&b, &t));

    let mut out: Vec<u8> = Vec::with_capacity(base.len().max(o.len()));
    let mut at = 0usize; // next base line to copy
    let (mut i, mut j) = (0usize, 0usize);
    let emit = |out: &mut Vec<u8>, lines: &[&[u8]]| lines.iter().for_each(|l| out.extend_from_slice(l));
    while i < ours.len() || j < theirs.len() {
        let (x, y) = (ours.get(i), theirs.get(j));
        // Which hunk comes first; at the same start, an insertion goes
        // before a replacement, whoever made it.
        let key = |h: &Hunk<'_>| (h.start, h.end != h.start);
        let take_ours = match (x, y) {
            (Some(x), Some(y)) => {
                let overlap = (x.start < y.end && y.start < x.end)
                    || (x.start == y.start && (x.start == x.end) == (y.start == y.end));
                if overlap {
                    // The same change on both sides counts once; anything
                    // else on the same lines is a conflict.
                    if x.start == y.start && x.end == y.end && x.lines == y.lines {
                        emit(&mut out, &b[at..x.start]);
                        emit(&mut out, x.lines);
                        at = x.end;
                        i += 1;
                        j += 1;
                        continue;
                    }
                    return None;
                }
                key(x) <= key(y)
            }
            (Some(_), None) => true,
            (None, _) => false,
        };
        let hunk = if take_ours {
            i += 1;
            &ours[i - 1]
        } else {
            j += 1;
            &theirs[j - 1]
        };
        if hunk.start < at {
            return None; // overlaps what the other side already replaced
        }
        emit(&mut out, &b[at..hunk.start]);
        emit(&mut out, hunk.lines);
        at = hunk.end;
    }
    emit(&mut out, &b[at..]);
    Some(out)
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

    #[test]
    fn changes_to_different_lines_merge() {
        let base = b"a\nb\nc\nd\ne\n";
        // Far apart, touching, an insertion right before a replaced line.
        assert_eq!(merge3(base, b"A\nb\nc\nd\ne\n", b"a\nb\nc\nd\nE\n").unwrap(), b"A\nb\nc\nd\nE\n");
        assert_eq!(merge3(base, b"a\nB\nc\nd\ne\n", b"a\nb\nC\nd\ne\n").unwrap(), b"a\nB\nC\nd\ne\n");
        assert_eq!(
            merge3(base, b"a\nb\nnew\nc\nd\ne\n", b"a\nb\nC\nd\ne\n").unwrap(),
            b"a\nb\nnew\nC\nd\ne\n"
        );
        assert_eq!(
            merge3(base, b"a\nb\nC\nd\ne\n", b"a\nb\nnew\nc\nd\ne\n").unwrap(),
            b"a\nb\nnew\nC\nd\ne\n"
        );
        // A removal on one side, an addition at the end on the other.
        assert_eq!(merge3(base, b"a\nc\nd\ne\n", b"a\nb\nc\nd\ne\nf\n").unwrap(), b"a\nc\nd\ne\nf\n");
        // One side did nothing; both did the same.
        assert_eq!(merge3(base, base, b"x\n").unwrap(), b"x\n");
        assert_eq!(merge3(base, b"x\n", base).unwrap(), b"x\n");
        assert_eq!(merge3(base, b"a\nB\nc\nd\ne\n", b"a\nB\nc\nd\ne\n").unwrap(), b"a\nB\nc\nd\ne\n");
        assert_eq!(merge3(base, b"a\nB\nc\nd\nE\n", b"a\nB\nc\nD\ne\n").unwrap(), b"a\nB\nc\nD\nE\n");
    }

    #[test]
    fn changes_to_the_same_lines_conflict() {
        let base = b"a\nb\nc\n";
        assert_eq!(merge3(base, b"a\nB\nc\n", b"a\nbb\nc\n"), None);
        // Removed here, changed there.
        assert_eq!(merge3(base, b"a\nc\n", b"a\nB\nc\n"), None);
        // Two different insertions at the same point.
        assert_eq!(merge3(base, b"a\nx\nb\nc\n", b"a\ny\nb\nc\n"), None);
        // An insertion inside a block the other side replaced.
        assert_eq!(merge3(base, b"a\nb\nx\nc\n", b"a\nB\nC\n"), None);
        // Binary content.
        assert_eq!(merge3(&[0, 1], &[0, 2], &[0, 3]), None);
    }

    proptest! {
        #[test]
        fn merge3_applies_both_sides_and_is_symmetric(
            base in prop::collection::vec("[abc]{1,3}\n", 0..24),
            edits in prop::collection::vec((any::<prop::sample::Index>(), "[xyz]{1,3}\n", any::<bool>()), 0..6),
        ) {
            // Lines are told apart by their text alone: here each one is unique.
            let base: Vec<String> = base.iter().enumerate().map(|(i, line)| format!("{i} {line}")).collect();
            // Each edit replaces one base line, on one side or the other.
            let (mut ours, mut theirs, mut both) = (base.clone(), base.clone(), base.clone());
            if !base.is_empty() {
                for (at, text, mine) in &edits {
                    let at = at.index(base.len());
                    if both[at] != base[at] {
                        continue; // one edit per line: no conflicts in this test
                    }
                    let text = format!("{at} {text}");
                    both[at] = text.clone();
                    if *mine { ours[at] = text } else { theirs[at] = text }
                }
            }
            let (base, ours, theirs, both) = (base.concat(), ours.concat(), theirs.concat(), both.concat());
            let merged = merge3(base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
            prop_assert_eq!(merged.as_deref(), Some(both.as_bytes()));
            prop_assert_eq!(merge3(base.as_bytes(), theirs.as_bytes(), ours.as_bytes()), merged);
        }

        #[test]
        fn merge3_never_invents_or_loses_a_one_sided_change(
            base in prop::collection::vec("[abc]{0,3}\n?", 0..20),
            ours in prop::collection::vec("[abc]{0,3}\n?", 0..20),
            theirs in prop::collection::vec("[abc]{0,3}\n?", 0..20),
        ) {
            let (base, ours, theirs) = (base.concat(), ours.concat(), theirs.concat());
            let (b, o, t) = (base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
            prop_assert_eq!(merge3(b, o, b), Some(o.to_vec()));
            prop_assert_eq!(merge3(b, b, t), Some(t.to_vec()));
            prop_assert_eq!(merge3(b, o, o), Some(o.to_vec()));
            // Whatever the inputs: a result or a conflict, the same from both sides.
            prop_assert_eq!(merge3(b, o, t), merge3(b, t, o));
        }

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
