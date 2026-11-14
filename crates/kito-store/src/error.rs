//! Errori dello store.

use kito_core::CodecError;
use std::io;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("I/O su {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{} non è uno store Kito (manca o è illeggibile il file `format`)", .0.display())]
    NotAStore(PathBuf),

    #[error("{} contiene già uno store Kito", .0.display())]
    AlreadyExists(PathBuf),

    #[error("formato dello store non supportato: {0:?}")]
    UnsupportedFormat(String),

    #[error("lo store {} è già aperto in scrittura da un altro processo", .0.display())]
    Locked(PathBuf),

    /// Dati confermati danneggiati: non si ripara in automatico.
    #[error("dati corrotti in {} all'offset {offset}: {reason}", path.display())]
    Corrupt { path: PathBuf, offset: u64, reason: String },

    #[error("lo store ha subito un errore di scrittura e va riaperto")]
    Poisoned,

    #[error("record troppo grande: {0} byte")]
    RecordTooLarge(usize),

    #[error("operazione rifiutata: {0}")]
    InvalidOp(String),

    #[error(transparent)]
    Codec(#[from] CodecError),

    #[error("indice: {0}")]
    Index(String),
}

impl StoreError {
    pub(crate) fn corrupt(path: &Path, offset: u64, reason: impl Into<String>) -> Self {
        StoreError::Corrupt { path: path.to_owned(), offset, reason: reason.into() }
    }

    /// `true` se l'errore è "spazio su disco esaurito".
    pub fn is_storage_full(&self) -> bool {
        matches!(self, StoreError::Io { source, .. } if source.kind() == io::ErrorKind::StorageFull)
    }
}

/// Aggiunge il percorso a un errore di I/O.
pub(crate) trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| StoreError::Io { path: path.to_owned(), source })
    }
}

macro_rules! index_error {
    ($($t:ty),*) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self {
                StoreError::Index(e.to_string())
            }
        }
    )*};
}

index_error!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::SetDurabilityError
);
