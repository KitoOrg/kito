//! `kito-core`: Kito's data model types and their serialization.
//!
//! The data model is the project's contract (see `docs/adr/`). Rules for
//! anyone who changes these types:
//!
//! * fields may be **added**, only at the end and only as optional
//!   (`#[serde(default, skip_serializing_if = "Option::is_none")]`): that way
//!   records already written stay readable and hashes already computed do not
//!   change;
//! * fields and variants of the types that go into a hash ([`Op`],
//!   [`OpKind`], [`Symbol`], [`Intent`], [`Author`]…) are never renamed or
//!   reordered;
//! * changing the semantics of a type requires an ADR first.

pub mod codec;
pub mod edit;
pub mod hash;
pub mod hlc;
pub mod ids;
pub mod model;
mod serde_helpers;
pub mod symbols;
pub mod tree;

pub use codec::CodecError;
pub use edit::{apply_edits, EditError, TextEdit};
pub use hash::{Hash, HashParseError};
pub use hlc::{Hlc, HlcClock};
pub use ids::{BlobId, CheckpointId, ConflictId, IdParseError, IntentId, LaneId, OpId, SymbolId};
pub use model::{
    Author, AuthorKind, Checkpoint, Conflict, ConflictKind, FileMode, Fingerprint, Intent, Lane, LaneState,
    Lang, Location, Op, OpKind, Proposal, RefSource, Reference, Resolution, ScopeEntry, ScopeItem, Symbol,
    SymbolKind, Verification,
};
pub use symbols::{SymbolEntry, SymbolError, SymbolTable};
pub use tree::{Tree, TreeEntry, TreeError};
