//! Positions as language servers count them: a line and a number of UTF-16
//! code units in it.

/// The line (from 0) and UTF-16 column of a byte offset.
pub fn position_at(content: &[u8], offset: usize) -> (u32, u32) {
    let offset = offset.min(content.len());
    let line_start = content[..offset].iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1);
    let line = content[..line_start].iter().filter(|&&b| b == b'\n').count() as u32;
    let column: usize =
        String::from_utf8_lossy(&content[line_start..offset]).chars().map(char::len_utf16).sum();
    (line, column as u32)
}

/// The byte offset of a line (from 0) and a UTF-16 column. A column past the
/// end of the line is the end of the line; `None` if the line is not there.
pub fn offset_at(content: &[u8], line: u32, column: u32) -> Option<usize> {
    let mut start = 0;
    for _ in 0..line {
        start += content[start..].iter().position(|&b| b == b'\n')? + 1;
    }
    let end = content[start..].iter().position(|&b| b == b'\n').map_or(content.len(), |at| start + at);
    let text = String::from_utf8_lossy(&content[start..end]);
    let mut units = 0;
    for (at, c) in text.char_indices() {
        if units >= column as usize {
            return Some(start + at);
        }
        units += c.len_utf16();
    }
    Some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_utf16_units() {
        let text = "let a = 1;\nlet città = \"𝄞\"; x\n\nend".as_bytes();
        let x = text.iter().position(|&b| b == b'x').unwrap();
        // `à` is two bytes and one unit, `𝄞` four bytes and two units.
        assert_eq!(position_at(text, x), (1, 18));
        assert_eq!(offset_at(text, 1, 18), Some(x));
        assert_eq!(position_at(text, 0), (0, 0));
        assert_eq!(offset_at(text, 0, 0), Some(0));
        assert_eq!(offset_at(text, 3, 3), Some(text.len()));
        assert_eq!(position_at(text, text.len()), (3, 3));
        // Past the end of a line, and past the end of the file.
        assert_eq!(offset_at(text, 0, 99), Some(10));
        assert_eq!(offset_at(text, 2, 0), Some(text.len() - 4));
        assert_eq!(offset_at(text, 9, 0), None);
    }
}
