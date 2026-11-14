//! Identificatori del modello dati.
//!
//! Tutto è indirizzato per contenuto ([`BlobId`], [`OpId`], [`IntentId`]),
//! tranne gli ID dei simboli ([`SymbolId`]), assegnati alla nascita e mai più
//! cambiati, e i contatori locali al repository ([`LaneId`], [`CheckpointId`],
//! [`ConflictId`]).

use crate::hash::{Hash, HashParseError};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdParseError {
    #[error("identificatore non valido: {0:?}")]
    Malformed(String),
    #[error(transparent)]
    Hash(#[from] HashParseError),
}

fn malformed(s: &str) -> IdParseError {
    IdParseError::Malformed(s.to_owned())
}

/// Identificatore di una lane. `LaneId::MAIN` è la linea principale.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LaneId(pub u64);

impl LaneId {
    pub const MAIN: LaneId = LaneId(0);

    pub const fn is_main(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for LaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_main() {
            f.write_str("lane/main")
        } else {
            write!(f, "lane/{:x}", self.0)
        }
    }
}

impl fmt::Debug for LaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for LaneId {
    type Err = IdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s.strip_prefix("lane/").unwrap_or(s);
        if rest == "main" {
            return Ok(LaneId::MAIN);
        }
        u64::from_str_radix(rest, 16).map(LaneId).map_err(|_| malformed(s))
    }
}

/// ID permanente di un simbolo. Non cambia con rename, move o modifiche.
/// (lane che l'ha creato, contatore locale) → unico senza coordinamento.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SymbolId {
    origin: LaneId,
    seq: u64,
}

impl SymbolId {
    pub const fn new(origin: LaneId, seq: u64) -> Self {
        SymbolId { origin, seq }
    }

    /// Lane che ha creato il simbolo.
    pub const fn origin(self) -> LaneId {
        self.origin
    }

    /// Contatore locale alla lane di origine.
    pub const fn seq(self) -> u64 {
        self.seq
    }
}

impl fmt::Display for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}#{}", self.origin.0, self.seq)
    }
}

impl fmt::Debug for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SymbolId({self})")
    }
}

impl FromStr for SymbolId {
    type Err = IdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (origin, seq) = s.split_once('#').ok_or_else(|| malformed(s))?;
        Ok(SymbolId {
            origin: LaneId(u64::from_str_radix(origin, 16).map_err(|_| malformed(s))?),
            seq: seq.parse().map_err(|_| malformed(s))?,
        })
    }
}

macro_rules! hash_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Hash);

        impl $name {
            pub const fn hash(&self) -> &Hash {
                &self.0
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                self.0.as_bytes()
            }

            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                $name(Hash::from_bytes(bytes))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0.short())
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok($name(Hash::from_hex(s)?))
            }
        }
    };
}

hash_id!(
    /// Hash BLAKE3 del contenuto di un blob.
    BlobId
);
hash_id!(
    /// Hash di (parents, kind, payload, author) di un'operazione.
    OpId
);
hash_id!(
    /// Hash del contenuto di un [`crate::Intent`].
    IntentId
);

impl BlobId {
    /// ID del blob con questo contenuto.
    pub fn of(data: &[u8]) -> Self {
        BlobId(Hash::of(data))
    }
}

macro_rules! counter_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "#{}"), self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let rest = s.strip_prefix(concat!($prefix, "#")).unwrap_or(s);
                rest.parse().map($name).map_err(|_| malformed(s))
            }
        }
    };
}

counter_id!(
    /// Numero progressivo di un checkpoint (`ckpt#119`).
    CheckpointId,
    "ckpt"
);
counter_id!(
    /// Numero progressivo di un conflitto (`conflict#7`).
    ConflictId,
    "conflict"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_parse_round_trip() {
        assert_eq!(LaneId::MAIN.to_string(), "lane/main");
        assert_eq!(LaneId(0xb2).to_string(), "lane/b2");
        assert_eq!("lane/b2".parse::<LaneId>().unwrap(), LaneId(0xb2));
        assert_eq!("main".parse::<LaneId>().unwrap(), LaneId::MAIN);
        assert!("lane/zz".parse::<LaneId>().is_err());

        let s = SymbolId::new(LaneId(0xb2), 42);
        assert_eq!(s.to_string(), "b2#42");
        assert_eq!(s.to_string().parse::<SymbolId>().unwrap(), s);
        assert!("b2".parse::<SymbolId>().is_err());

        assert_eq!(CheckpointId(119).to_string(), "ckpt#119");
        assert_eq!("ckpt#119".parse::<CheckpointId>().unwrap(), CheckpointId(119));
        assert_eq!("119".parse::<CheckpointId>().unwrap(), CheckpointId(119));
        assert_eq!(ConflictId(7).to_string(), "conflict#7");

        let op = OpId(Hash::of(b"x"));
        assert_eq!(op.to_string().parse::<OpId>().unwrap(), op);
    }
}
