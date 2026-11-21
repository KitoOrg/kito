//! Identifiers of the data model.
//!
//! Everything is content-addressed ([`BlobId`], [`OpId`], [`IntentId`]),
//! except symbol IDs ([`SymbolId`]), assigned at birth and never changed
//! afterwards, and the repository-local counters ([`LaneId`],
//! [`CheckpointId`], [`ConflictId`]).

use crate::hash::{Hash, HashParseError};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdParseError {
    #[error("invalid identifier: {0:?}")]
    Malformed(String),
    #[error(transparent)]
    Hash(#[from] HashParseError),
}

fn malformed(s: &str) -> IdParseError {
    IdParseError::Malformed(s.to_owned())
}

/// Identifier of a lane. `LaneId::MAIN` is the main lane.
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

/// Permanent ID of a symbol. It does not change on rename, move or edits.
/// (lane that created it, local counter) → unique without coordination.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SymbolId {
    origin: LaneId,
    seq: u64,
}

impl SymbolId {
    pub const fn new(origin: LaneId, seq: u64) -> Self {
        SymbolId { origin, seq }
    }

    /// Lane that created the symbol.
    pub const fn origin(self) -> LaneId {
        self.origin
    }

    /// Counter local to the origin lane.
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
    /// BLAKE3 hash of the content of a blob.
    BlobId
);
hash_id!(
    /// Hash of (parents, kind, payload, author) of an operation.
    OpId
);
hash_id!(
    /// Hash of the content of an [`crate::Intent`].
    IntentId
);

impl BlobId {
    /// ID of the blob with this content.
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
    /// Sequence number of a checkpoint (`ckpt#119`).
    CheckpointId,
    "ckpt"
);
counter_id!(
    /// Sequence number of a conflict (`conflict#7`).
    ConflictId,
    "conflict"
);
counter_id!(
    /// Sequence number of a note on code (`note#3`, ADR 0003).
    NoteId,
    "note"
);
counter_id!(
    /// Sequence number of a comment on a lane (`comment#3`, ADR 0004).
    CommentId,
    "comment"
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
