//! `kito-store`: Kito's on-disk store.
//!
//! ```text
//! .kito/
//!   format        version of the store format
//!   lock          exclusive lock of the single writer
//!   objects/      zstd-compressed blobs, addressed by BLAKE3, in append-only packs
//!   oplog/        append-only segments; record = length + payload + CRC32C
//!   index/        rebuildable indexes (redb)
//!   lanes/<id>/   lane state (managed by `kito-lane`)
//!   checkpoints   append-only log of the checkpoints
//!   intents       append-only log of the intents (the "why" of the ops)
//!   lost+found/   log tails discarded by recovery after a crash
//! ```
//!
//! Two invariants:
//!
//! 1. the indexes can always be rebuilt from `objects/` and `oplog/`;
//! 2. a crash during a write leaves at most one torn final record, which is
//!    discarded on reopening thanks to the CRC. Everything that
//!    [`Store::commit`] has confirmed survives.
//!
//! `unsafe` appears only in the `mmap` module.

mod checkpoints;
mod error;
pub mod fs;
mod index;
mod intents;
mod logfile;
#[allow(unsafe_code)]
mod mmap;
mod objects;
mod record;
mod segments;
mod store;
pub mod testing;

pub use error::{Result, StoreError};
pub use logfile::LogRecovery;
pub use objects::MAX_BLOB;
pub use record::{MAX_PAYLOAD, RECORD_OVERHEAD};
pub use segments::Pos;
pub use store::{
    IndexRecovery, LaneSummary, RecoveryReport, Store, StoreOptions, StoreStats, VerifyReport, STORE_DIR,
};
