//! 256-bit BLAKE3 content hash.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Content hash: BLAKE3, 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash([u8; 32]);

/// Error parsing a hash in hexadecimal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HashParseError {
    #[error("invalid length: expected {expected} hexadecimal characters, found {found}")]
    Length { expected: usize, found: usize },
    #[error("non-hexadecimal character at position {0}")]
    Digit(usize),
}

impl Hash {
    pub const LEN: usize = 32;
    pub const ZERO: Hash = Hash([0; 32]);

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Hash(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// BLAKE3 of the content, with no context: it matches `b3sum`.
    pub fn of(data: &[u8]) -> Self {
        Hash(*blake3::hash(data).as_bytes())
    }

    /// BLAKE3 in `derive_key` mode: it separates the domains, so the hash of
    /// an operation can never collide with that of a blob.
    pub fn derive(context: &str, data: &[u8]) -> Self {
        Hash(blake3::derive_key(context, data))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(&self.0)
    }

    /// Short prefix for human output (12 hexadecimal characters).
    pub fn short(&self) -> String {
        hex::encode(&self.0[..6])
    }

    pub fn from_hex(s: &str) -> Result<Self, HashParseError> {
        let mut out = [0u8; 32];
        hex::decode_into(s, &mut out)?;
        Ok(Hash(out))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::str::FromStr for Hash {
    type Err = HashParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Hash::from_hex(s)
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&self.to_hex())
        } else {
            s.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut out = [0u8; 32];
        hex::deserialize_fixed(d, &mut out)?;
        Ok(Hash(out))
    }
}

/// Minimal, dependency-free hexadecimal, for hashes and git object ids.
pub(crate) mod hex {
    use super::*;

    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push(DIGITS[(b >> 4) as usize] as char);
            s.push(DIGITS[(b & 0xf) as usize] as char);
        }
        s
    }

    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }

    pub fn decode_into(s: &str, out: &mut [u8]) -> Result<(), HashParseError> {
        let s = s.as_bytes();
        if s.len() != out.len() * 2 {
            return Err(HashParseError::Length { expected: out.len() * 2, found: s.len() });
        }
        for (i, o) in out.iter_mut().enumerate() {
            let hi = nibble(s[2 * i]).ok_or(HashParseError::Digit(2 * i))?;
            let lo = nibble(s[2 * i + 1]).ok_or(HashParseError::Digit(2 * i + 1))?;
            *o = (hi << 4) | lo;
        }
        Ok(())
    }

    /// Deserializes `out.len()` bytes from a hexadecimal string
    /// (human-readable formats), from a byte string (CBOR) or from a sequence
    /// of integers.
    pub fn deserialize_fixed<'de, D: Deserializer<'de>>(d: D, out: &mut [u8]) -> Result<(), D::Error> {
        struct V<'a>(&'a mut [u8]);

        impl<'de> de::Visitor<'de> for V<'_> {
            type Value = ();

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{} bytes (byte string or hexadecimal)", self.0.len())
            }

            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<(), E> {
                if v.len() != self.0.len() {
                    return Err(E::invalid_length(v.len(), &self));
                }
                self.0.copy_from_slice(v);
                Ok(())
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<(), E> {
                decode_into(v, self.0).map_err(E::custom)
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
                let len = self.0.len();
                for (i, o) in self.0.iter_mut().enumerate() {
                    *o = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(i, &"more bytes"))?;
                }
                if seq.next_element::<u8>()?.is_some() {
                    return Err(de::Error::invalid_length(len + 1, &"fewer bytes"));
                }
                Ok(())
            }
        }

        if d.is_human_readable() {
            d.deserialize_str(V(out))
        } else {
            d.deserialize_bytes(V(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_blake3_reference_vector() {
        // Official test vector: BLAKE3("").
        assert_eq!(
            Hash::of(b"").to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn hex_round_trip_and_errors() {
        let h = Hash::of(b"kito");
        assert_eq!(Hash::from_hex(&h.to_hex()).unwrap(), h);
        assert_eq!(h.to_hex().to_uppercase().parse::<Hash>().unwrap(), h);
        assert_eq!(h.short().len(), 12);
        assert!(matches!(Hash::from_hex("abc"), Err(HashParseError::Length { .. })));
        let bad = format!("zz{}", &h.to_hex()[2..]);
        assert_eq!(Hash::from_hex(&bad), Err(HashParseError::Digit(0)));
    }

    #[test]
    fn derive_separates_domains() {
        assert_ne!(Hash::derive("kito a", b"x"), Hash::derive("kito b", b"x"));
        assert_ne!(Hash::derive("kito a", b"x"), Hash::of(b"x"));
    }
}
