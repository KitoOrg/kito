//! Helper serde condivisi dal modello dati.

/// `Vec<u8>` come byte string nei formati binari; nei formati leggibili come
/// stringa se è UTF-8 valido, altrimenti come sequenza di interi (senza perdita).
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
                f.write_str("byte string, stringa o sequenza di byte")
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

/// `Option<[u8; 20]>` (object id di git): esadecimale nei formati leggibili,
/// byte string in quelli binari.
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
