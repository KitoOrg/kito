//! Modifiche testuali a intervalli di byte.

use serde::{Deserialize, Serialize};

/// Sostituisce i byte `start..end` del testo *di partenza* con `text`.
///
/// In una lista di edit gli intervalli sono ordinati, non si sovrappongono e
/// sono tutti espressi rispetto al testo di partenza (non a quello già
/// modificato dagli edit precedenti).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TextEdit {
    pub start: u32,
    pub end: u32,
    #[serde(with = "crate::serde_helpers::bytes")]
    pub text: Vec<u8>,
}

impl TextEdit {
    pub fn new(start: u32, end: u32, text: impl Into<Vec<u8>>) -> Self {
        TextEdit { start, end, text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("edit {index}: intervallo {start}..{end} non valido per un testo di {len} byte")]
    OutOfBounds { index: usize, start: u32, end: u32, len: usize },
    #[error("edit {index}: si sovrappone al precedente o non è in ordine")]
    Overlap { index: usize },
}

/// Applica gli edit a `before` e restituisce il testo risultante.
pub fn apply_edits(before: &[u8], edits: &[TextEdit]) -> Result<Vec<u8>, EditError> {
    let grow: usize = edits.iter().map(|e| e.text.len()).sum();
    let mut out = Vec::with_capacity(before.len() + grow);
    let mut cursor = 0usize;
    for (index, e) in edits.iter().enumerate() {
        let (start, end) = (e.start as usize, e.end as usize);
        if start > end || end > before.len() {
            return Err(EditError::OutOfBounds { index, start: e.start, end: e.end, len: before.len() });
        }
        if start < cursor {
            return Err(EditError::Overlap { index });
        }
        out.extend_from_slice(&before[cursor..start]);
        out.extend_from_slice(&e.text);
        cursor = end;
    }
    out.extend_from_slice(&before[cursor..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_in_order_against_original_offsets() {
        let before = b"hello brave new world";
        let edits = [TextEdit::new(0, 5, "HELLO"), TextEdit::new(6, 12, ""), TextEdit::new(21, 21, "!")];
        assert_eq!(apply_edits(before, &edits).unwrap(), b"HELLO new world!");
        assert_eq!(apply_edits(before, &[]).unwrap(), before);
    }

    #[test]
    fn rejects_invalid_edits() {
        assert!(matches!(
            apply_edits(b"abc", &[TextEdit::new(2, 9, "")]),
            Err(EditError::OutOfBounds { index: 0, .. })
        ));
        assert!(matches!(
            apply_edits(b"abc", &[TextEdit::new(2, 1, "")]),
            Err(EditError::OutOfBounds { .. })
        ));
        assert_eq!(
            apply_edits(b"abcdef", &[TextEdit::new(0, 3, "x"), TextEdit::new(2, 4, "y")]),
            Err(EditError::Overlap { index: 1 })
        );
    }
}
