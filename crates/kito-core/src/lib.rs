//! `kito-core`: tipi del modello dati di Kito e loro serializzazione.
//!
//! Il modello dati è il contratto del progetto (vedi `docs/adr/`). Regole per
//! chi modifica questi tipi:
//!
//! * si possono **aggiungere** campi, solo in coda e solo opzionali
//!   (`#[serde(default, skip_serializing_if = "Option::is_none")]`): così i
//!   record già scritti restano leggibili e gli hash già calcolati non cambiano;
//! * non si rinominano né si riordinano campi e varianti dei tipi che entrano
//!   in un hash ([`Op`], [`OpKind`], [`Symbol`], [`Intent`], [`Author`]…);
//! * per cambiare la semantica di un tipo serve prima un ADR.

pub mod codec;
pub mod edit;
pub mod hash;
pub mod hlc;
pub mod ids;
pub mod model;
mod serde_helpers;

pub use codec::CodecError;
pub use edit::{apply_edits, EditError, TextEdit};
pub use hash::{Hash, HashParseError};
pub use hlc::{Hlc, HlcClock};
pub use ids::{BlobId, CheckpointId, ConflictId, IdParseError, IntentId, LaneId, OpId, SymbolId};
pub use model::{
    Author, AuthorKind, Checkpoint, Conflict, ConflictKind, Fingerprint, Intent, Lane, LaneState, Lang,
    Location, Op, OpKind, RefSource, Reference, Symbol, SymbolKind, Verification,
};
