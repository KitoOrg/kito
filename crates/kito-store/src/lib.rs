//! `kito-store`: lo store su disco di Kito.
//!
//! ```text
//! .kito/
//!   format        versione del formato dello store
//!   lock          lock esclusivo dell'unico scrittore
//!   objects/      blob compressi zstd, indirizzati con BLAKE3, in pack append-only
//!   oplog/        segmenti append-only; record = lunghezza + payload + CRC32C
//!   index/        indici ricostruibili (redb)
//!   lanes/<id>/   stato delle lane (gestito da `kito-lane`)
//!   checkpoints   log append-only dei checkpoint
//!   lost+found/   code di log scartate dal recupero dopo un crash
//! ```
//!
//! Due invarianti:
//!
//! 1. gli indici si possono sempre ricostruire da `objects/` e `oplog/`;
//! 2. un crash durante una scrittura lascia al massimo un record finale
//!    troncato, che alla riapertura viene scartato grazie al CRC. Tutto ciò
//!    che [`Store::commit`] ha confermato sopravvive.
//!
//! `unsafe` compare solo nel modulo `mmap`.

mod checkpoints;
mod error;
pub mod fs;
mod index;
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
