//! Serde helpers shared by the data model.

/// `Vec<u8>` as a byte string in binary formats; in human-readable formats as
/// a string if it is valid UTF-8, otherwise as a sequence of integers (lossless).
pub(crate) mod bytes {
    use serde::{de, Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            match std::str::from_utf8(v) {
                Ok(text) => s.serialize_str(text),
                Err(_) => s.collect_seq(v.iter()),
            }
        } else {
            s.serialize_bytes(v)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct V;

        impl<'de> de::Visitor<'de> for V {
            type Value = Vec<u8>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte string, a string or a sequence of bytes")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<u8>, E> {
                Ok(v.as_bytes().to_vec())
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1 << 16));
                while let Some(b) = seq.next_element()? {
                    out.push(b);
                }
                Ok(out)
            }
        }

        if d.is_human_readable() {
            d.deserialize_any(V)
        } else {
            d.deserialize_byte_buf(V)
        }
    }
}

/// `Option<[u8; 20]>` (git object id): hexadecimal in human-readable formats,
/// byte string in binary ones.
pub(crate) mod opt_oid20 {
    use crate::hash::hex;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    struct Oid([u8; 20]);

    impl Serialize for Oid {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            if s.is_human_readable() {
                s.serialize_str(&hex::encode(&self.0))
            } else {
                s.serialize_bytes(&self.0)
            }
        }
    }

    impl<'de> Deserialize<'de> for Oid {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let mut out = [0u8; 20];
            hex::deserialize_fixed(d, &mut out)?;
            Ok(Oid(out))
        }
    }

    pub fn serialize<S: Serializer>(v: &Option<[u8; 20]>, s: S) -> Result<S::Ok, S::Error> {
        v.map(Oid).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 20]>, D::Error> {
        Ok(Option::<Oid>::deserialize(d)?.map(|o| o.0))
    }
}
