//! Canonical binary encoding: CBOR (RFC 8949).
//!
//! It is the format of the on-disk records and the input of the content
//! hashes. It is self-describing (structs are maps keyed by field name), so
//! adding an optional field breaks neither the reading of old records nor the
//! hashes already computed. Rationale in `docs/decisions.md`.

use serde::{de::DeserializeOwned, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("CBOR encoding failed: {0}")]
    Encode(String),
    #[error("CBOR decoding failed: {0}")]
    Decode(String),
    #[error("{0} trailing bytes after the CBOR value")]
    Trailing(usize),
}

/// Serializes to CBOR. Fails only for values that cannot be represented
/// (for example a non-UTF-8 path).
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::with_capacity(128);
    to_writer(value, &mut out)?;
    Ok(out)
}

/// Like [`to_vec`], but appends to an existing buffer.
pub fn to_writer<T: Serialize + ?Sized>(value: &T, out: &mut Vec<u8>) -> Result<(), CodecError> {
    ciborium::ser::into_writer(value, out).map_err(|e| CodecError::Encode(e.to_string()))
}

/// Deserializes exactly one CBOR value: trailing bytes are an error.
pub fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    let mut rest = bytes;
    let value = ciborium::de::from_reader(&mut rest).map_err(|e| CodecError::Decode(e.to_string()))?;
    if !rest.is_empty() {
        return Err(CodecError::Trailing(rest.len()));
    }
    Ok(value)
}
