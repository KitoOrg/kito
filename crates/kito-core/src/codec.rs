//! Codifica binaria canonica: CBOR (RFC 8949).
//!
//! È il formato dei record su disco e l'input degli hash di contenuto. È
//! autodescrittivo (le struct sono mappe con i nomi dei campi), quindi
//! aggiungere un campo opzionale non rompe né la lettura dei record vecchi né
//! gli hash già calcolati. Motivazione in `docs/decisions.md`.

use serde::{de::DeserializeOwned, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("codifica CBOR fallita: {0}")]
    Encode(String),
    #[error("decodifica CBOR fallita: {0}")]
    Decode(String),
    #[error("{0} byte in eccesso dopo il valore CBOR")]
    Trailing(usize),
}

/// Serializza in CBOR. Fallisce solo per valori non rappresentabili
/// (per esempio un percorso non UTF-8).
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::with_capacity(128);
    to_writer(value, &mut out)?;
    Ok(out)
}

/// Come [`to_vec`], ma accoda a un buffer esistente.
pub fn to_writer<T: Serialize + ?Sized>(value: &T, out: &mut Vec<u8>) -> Result<(), CodecError> {
    ciborium::ser::into_writer(value, out).map_err(|e| CodecError::Encode(e.to_string()))
}

/// Deserializza esattamente un valore CBOR: byte in eccesso sono un errore.
pub fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    let mut rest = bytes;
    let value = ciborium::de::from_reader(&mut rest).map_err(|e| CodecError::Decode(e.to_string()))?;
    if !rest.is_empty() {
        return Err(CodecError::Trailing(rest.len()));
    }
    Ok(value)
}
