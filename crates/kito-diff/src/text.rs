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
#[derive(Clone, Copy)]
struct Hunk<'a> {
    start: usize,
    end: usize,
    /// Where `lines` starts among the lines of the side.
    from: usize,
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
                last.lines = &side[last.from..s.end];
            }
            _ => out.push(Hunk { start: b.start, end: b.end, from: s.start, lines: &side[s] }),
        }
    }
    out
}

/// The `n`-th change of a side, moved down as far as it goes without
/// changing what it makes of the text. Lines that are only added, or only
/// removed, next to lines that repeat (a blank line, a closing brace) can
/// be placed in several ways, and a diff picks one by what else changed:
/// moved down, the same change is the same hunk on both sides. It stops
/// where the next change of the side starts.
fn moved_down<'a>(base: &[&[u8]], side: &'a [&'a [u8]], hunks: &[Hunk<'a>], n: usize) -> Hunk<'a> {
    let hunk = hunks[n];
    let limit = hunks.get(n + 1).map_or(base.len(), |next| next.start);
    let mut by = 0;
    if hunk.start == hunk.end {
        // Added lines go past a line of the base that is their own first one.
        while hunk.start + by < limit && base[hunk.start + by] == side[hunk.from + by] {
            by += 1;
        }
        let from = hunk.from + by;
        return Hunk {
            start: hunk.start + by,
            end: hunk.end + by,
            from,
            lines: &side[from..from + hunk.lines.len()],
        };
    }
    if hunk.lines.is_empty() {
        // Removed lines, past a line that is the first of them.
        while hunk.end + by < limit && base[hunk.end + by] == base[hunk.start + by] {
            by += 1;
        }
    }
    Hunk { start: hunk.start + by, end: hunk.end + by, ..hunk }
}

/// `true` if two changes, one of each side, are the same lines added or the
/// same lines removed, however the two diffs placed them.
fn same_moved_down<'a>(
    base: &[&[u8]],
    ours: (&'a [&'a [u8]], &[Hunk<'a>], usize),
    theirs: (&'a [&'a [u8]], &[Hunk<'a>], usize),
) -> bool {
    let (x, y) = (&ours.1[ours.2], &theirs.1[theirs.2]);
    let adds = |h: &Hunk<'_>| h.start == h.end;
    let removes = |h: &Hunk<'_>| h.lines.is_empty();
    if !(adds(x) && adds(y)) && !(removes(x) && removes(y)) {
        return false;
    }
    let (x, y) = (moved_down(base, ours.0, ours.1, ours.2), moved_down(base, theirs.0, theirs.1, theirs.2));
    x.start == y.start && x.end == y.end && x.lines == y.lines
}

/// How many lines the end of `before` and the start of `after` have in common.
fn shared<'a>(before: &[&'a [u8]], after: &[&'a [u8]]) -> usize {
    let most = before.len().min(after.len());
    (1..=most).rev().find(|&n| before[before.len() - n..] == after[..n]).unwrap_or(0)
}

/// Three-way merge of text: the changes `ours` and `theirs` each made to
/// `base`, both applied. `None` if they cannot be: the two sides changed the
/// same lines in different ways, or the content is binary.
///
/// Changes to different lines never conflict, even when the lines touch;
/// the same change made on both sides is applied once, and so are the lines
/// both sides added where their changes touch. The result does not depend
/// on which side is called `ours`.
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
                         // What the last change put right before `at`: the next one, if it
                         // starts there, is the other side's.
    let mut before: &[&[u8]] = &[];
    let (mut i, mut j) = (0usize, 0usize);
    let emit = |out: &mut Vec<u8>, lines: &[&[u8]]| lines.iter().for_each(|l| out.extend_from_slice(l));
    while i < ours.len() || j < theirs.len() {
        let (x, y) = (ours.get(i), theirs.get(j));
        // Which hunk comes first; at the same start, an insertion goes
        // before a replacement, whoever made it.
        let key = |h: &Hunk<'_>| (h.start, h.end != h.start);
        let take_ours = match (x, y) {
            (Some(x), Some(y)) => {
                // The same lines added, or removed, by both sides: once,
                // wherever each diff placed them.
                if same_moved_down(&b, (&o, &ours, i), (&t, &theirs, j)) {
                    let first = if x.start <= y.start { x } else { y };
                    if first.start < at {
                        return None;
                    }
                    emit(&mut out, &b[at..first.start]);
                    emit(&mut out, first.lines);
                    at = first.end;
                    before = &[];
                    i += 1;
                    j += 1;
                    continue;
                }
                let overlap = (x.start < y.end && y.start < x.end)
                    || (x.start == y.start && (x.start == x.end) == (y.start == y.end));
                if overlap {
                    // The same change on both sides counts once; anything
                    // else on the same lines is a conflict.
                    if x.start == y.start && x.end == y.end && x.lines == y.lines {
                        emit(&mut out, &b[at..x.start]);
                        emit(&mut out, x.lines);
                        at = x.end;
                        before = &[];
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
        // Two changes that touch: the lines one ends with and the other
        // starts with were added by both at the same place, and are in the
        // result once ("b" → "B, new" here, "new" added after "b" there).
        let twice = match hunk.start == at {
            true => shared(before, hunk.lines),
            false => 0,
        };
        emit(&mut out, &b[at..hunk.start]);
        emit(&mut out, &hunk.lines[twice..]);
        at = hunk.end;
        before = &hunk.lines[twice..];
    }
    emit(&mut out, &b[at..]);
    Some(out)
}

/// Three-way merge of text as a plain line merge makes it (`git
/// merge-file`): the changes of the two sides are both applied only where
/// they neither overlap nor touch; where they do, the two sides must have
/// made the same thing of those lines. `None` otherwise, or if the content
/// is binary. Stricter than [`merge3`]: what it merges is beyond doubt.
pub fn merge3_apart(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
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
    // Each change where it is once moved down: the same change is then at
    // the same place on both sides.
    fn placed<'a>(base: &[&[u8]], side: &'a [&'a [u8]]) -> Vec<Hunk<'a>> {
        let found = hunks(base, side);
        (0..found.len()).map(|n| moved_down(base, side, &found, n)).collect()
    }
    let sides = [placed(&b, &o), placed(&b, &t)];

    let mut out: Vec<u8> = Vec::with_capacity(base.len().max(o.len()));
    let mut at = 0usize; // next base line to copy
    let mut next = [0usize, 0usize];
    let emit = |out: &mut Vec<u8>, lines: &[&[u8]]| lines.iter().for_each(|l| out.extend_from_slice(l));
    // What a side made of the base lines `[start, end)`, given its changes there.
    let made = |side: &[Hunk<'_>], start: usize, end: usize| -> Vec<u8> {
        let mut text = Vec::new();
        let mut from = start;
        for hunk in side {
            emit(&mut text, &b[from..hunk.start]);
            emit(&mut text, hunk.lines);
            from = hunk.end;
        }
        emit(&mut text, &b[from..end]);
        text
    };
    loop {
        // The next change, of either side, and every change that overlaps
        // or touches it or one already taken with it: one region of the base.
        let first = (0..2).filter_map(|s| sides[s].get(next[s]).map(|h| h.start)).min();
        let Some(start) = first else { break };
        let (from, mut end) = (next, start);
        loop {
            let mut grew = false;
            for s in 0..2 {
                while let Some(hunk) = sides[s].get(next[s]).filter(|h| h.start <= end) {
                    end = end.max(hunk.end);
                    next[s] += 1;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        let (mine, yours) = (&sides[0][from[0]..next[0]], &sides[1][from[1]..next[1]]);
        let text = match (mine.is_empty(), yours.is_empty()) {
            (false, true) => made(mine, start, end),
            (true, false) => made(yours, start, end),
            _ => {
                let (mine, yours) = (made(mine, start, end), made(yours, start, end));
                if mine != yours {
                    return None;
                }
                mine
            }
        };
        emit(&mut out, &b[at..start]);
        out.extend_from_slice(&text);
        at = end;
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

    #[test]
    fn lines_both_sides_added_where_their_changes_touch_are_there_once() {
        let base = b"one\ntwo\nthree\n";
        // One side changes a line and adds a line after it; the other adds
        // the same line at that place.
        let (changed, added) = (&b"one\nTWO\nadded\nthree\n"[..], &b"one\ntwo\nadded\nthree\n"[..]);
        assert_eq!(merge3(base, changed, added).unwrap(), changed);
        assert_eq!(merge3(base, added, changed).unwrap(), changed);
        // The same, with the line added before the changed one.
        let (changed, added) = (&b"one\nadded\nTWO\nthree\n"[..], &b"one\nadded\ntwo\nthree\n"[..]);
        assert_eq!(merge3(base, changed, added).unwrap(), changed);
        assert_eq!(merge3(base, added, changed).unwrap(), changed);
        // Each side changes a line, and both add the same lines in between.
        assert_eq!(
            merge3(base, b"one\nTWO\nx\ny\nthree\n", b"one\ntwo\nx\ny\nTHREE\n").unwrap(),
            b"one\nTWO\nx\ny\nTHREE\n"
        );
        // What only one side added there stays.
        assert_eq!(
            merge3(base, b"one\nTWO\nmore\nadded\nthree\n", b"one\ntwo\nadded\nthree\n").unwrap(),
            b"one\nTWO\nmore\nadded\nthree\n"
        );
        // The same line at two different places is two lines.
        assert_eq!(
            merge3(base, b"one\nadded\nTWO\nthree\n", b"one\ntwo\nadded\nthree\n").unwrap(),
            b"one\nadded\nTWO\nadded\nthree\n"
        );
    }

    #[test]
    fn the_same_lines_added_or_removed_by_both_sides_count_once_wherever_the_diff_puts_them() {
        // Functions end alike and are separated alike: the lines of a new
        // one can be told from the base in several ways.
        let function = |name: &str| format!("fn {name}() {{\n    let x = {name};\n    x\n}}\n");
        let join = |names: &[&str]| names.iter().map(|name| function(name)).collect::<Vec<_>>().join("\n");
        let base = join(&["f0", "f1", "f2", "f3"]);
        // Both sides add the same function; one also changes another.
        let ours = join(&["f0", "f1", "added", "f2", "f3"]);
        let theirs = ours.replace("let x = f0;", "changed();");
        let merged = merge3(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(merged).unwrap(), theirs);
        let merged = merge3(base.as_bytes(), theirs.as_bytes(), ours.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(merged).unwrap(), theirs);
        let merged = merge3_apart(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(merged).unwrap(), theirs);

        // Both sides remove the same function; one also changes another.
        let (base, ours) = (ours, base);
        let theirs = ours.replace("let x = f0;", "changed();");
        let merged = merge3(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(merged).unwrap(), theirs);
        let merged = merge3_apart(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(merged).unwrap(), theirs);
    }

    #[test]
    fn a_plain_line_merge_takes_only_changes_that_are_apart() {
        let base = b"a\nb\nc\nd\ne\n";
        assert_eq!(merge3_apart(base, b"A\nb\nc\nd\ne\n", b"a\nb\nc\nd\nE\n").unwrap(), b"A\nb\nc\nd\nE\n");
        assert_eq!(merge3_apart(base, b"a\nc\nd\ne\n", b"a\nb\nc\nd\ne\nf\n").unwrap(), b"a\nc\nd\ne\nf\n");
        // One line between two changes is enough.
        assert_eq!(merge3_apart(base, b"a\nB\nc\nd\ne\n", b"a\nb\nc\nD\ne\n").unwrap(), b"a\nB\nc\nD\ne\n");
        // Changes that touch are not merged, though `merge3` merges them.
        let (ours, theirs) = (&b"a\nB\nc\nd\ne\n"[..], &b"a\nb\nC\nd\ne\n"[..]);
        assert_eq!(merge3_apart(base, ours, theirs), None);
        assert!(merge3(base, ours, theirs).is_some());
        assert_eq!(merge3_apart(base, b"a\nb\nnew\nc\nd\ne\n", b"a\nb\nC\nd\ne\n"), None);
        assert_eq!(
            merge3_apart(b"one\ntwo\nthree\n", b"one\nTWO\nadded\nthree\n", b"one\ntwo\nadded\nthree\n"),
            None
        );
        assert_eq!(merge3_apart(base, b"a\nB\nc\nd\nE\n", b"a\nB\nc\nD\ne\n"), None);
        // Unless both sides made the same thing of those lines.
        assert_eq!(merge3_apart(base, b"a\nB\nC\nd\ne\n", b"a\nB\nC\nd\nE\n").unwrap(), b"a\nB\nC\nd\nE\n");
        assert_eq!(merge3_apart(base, base, b"x\n").unwrap(), b"x\n");
        assert_eq!(merge3_apart(base, b"x\n", base).unwrap(), b"x\n");
        assert_eq!(merge3_apart(&[0, 1], &[0, 2], &[0, 3]), None);

        // A function added at the end, after statements one of which the
        // other side changes.
        let base = b"def helper():\n    return 0\n\n\nstart('x')\nstart('z')\n";
        let ours =
            b"def helper():\n    return 0\n\n\nstart('x')\nstart('z')\n\n\ndef added():\n    return 1\n";
        let theirs = b"def helper():\n    return 0\n\n\nstart('y')\nstart('z')\n";
        assert_eq!(
            merge3_apart(base, ours, theirs).unwrap(),
            b"def helper():\n    return 0\n\n\nstart('y')\nstart('z')\n\n\ndef added():\n    return 1\n"
        );
    }

    proptest! {
        #[test]
        fn a_function_added_by_both_sides_at_one_place_is_there_once(
            functions in 3usize..8,
            at in any::<prop::sample::Index>(),
            far in any::<prop::sample::Index>(),
            line in 0usize..3,
        ) {
            // Functions that end alike and are separated alike, as code is.
            // Both sides add the same one between two of them; one side
            // also changes a line of another function.
            let function = |name: &str| format!("fn {name}() {{\n    let x = {name};\n    x\n}}\n");
            let base: Vec<String> = (0..functions).map(|i| function(&format!("f{i}"))).collect();
            let at = at.index(functions + 1);
            // Not one next to the new one: lines that end a function and
            // those that end the next can be taken for each other, and then
            // no line merge tells the new function from the changed one.
            let others: Vec<usize> = (0..functions).filter(|f| f + 1 < at || *f > at).collect();
            let far = others[far.index(others.len())];
            let mut ours = base.clone();
            ours.insert(at, function("added"));
            let mut theirs = base.clone();
            let changed: Vec<&str> = theirs[far].split_inclusive('\n').collect();
            theirs[far] = changed.iter().enumerate().map(|(n, l)| if n == line { "    changed();\n" } else { l }).collect();
            let mut both = theirs.clone();
            theirs.insert(at, function("added"));
            both.insert(at, function("added"));
            let (base, ours, theirs, both) = (base.join("\n"), ours.join("\n"), theirs.join("\n"), both.join("\n"));
            let (b, o, t) = (base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
            let merged = merge3(b, o, t).map(|merged| String::from_utf8_lossy(&merged).into_owned());
            prop_assert_eq!(merged.as_ref(), Some(&both));
            prop_assert_eq!(merge3(b, t, o), Some(both.clone().into_bytes()));
            prop_assert_eq!(merge3_apart(b, o, t), Some(both.into_bytes()));
        }

        #[test]
        fn merge3_apart_is_symmetric_and_never_merges_what_merge3_does_not(
            base in prop::collection::vec("[abc]{0,3}\n?", 0..20),
            ours in prop::collection::vec("[abc]{0,3}\n?", 0..20),
            theirs in prop::collection::vec("[abc]{0,3}\n?", 0..20),
        ) {
            let (base, ours, theirs) = (base.concat(), ours.concat(), theirs.concat());
            let (b, o, t) = (base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
            prop_assert_eq!(merge3_apart(b, o, b), Some(o.to_vec()));
            prop_assert_eq!(merge3_apart(b, b, t), Some(t.to_vec()));
            prop_assert_eq!(merge3_apart(b, o, o), Some(o.to_vec()));
            let apart = merge3_apart(b, o, t);
            prop_assert_eq!(&apart, &merge3_apart(b, t, o));
            if let Some(apart) = apart {
                prop_assert_eq!(merge3(b, o, t), Some(apart));
            }
        }

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
