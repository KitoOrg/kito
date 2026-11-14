//! Store errors.

use kito_core::CodecError;
use std::io;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("I/O on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{} is not a Kito store (the `format` file is missing or unreadable)", .0.display())]
    NotAStore(PathBuf),

    #[error("{} already contains a Kito store", .0.display())]
    AlreadyExists(PathBuf),

    #[error("unsupported store format: {0:?}")]
    UnsupportedFormat(String),

    #[error("the store {} is already open for writing by another process", .0.display())]
    Locked(PathBuf),

    /// Damaged confirmed data: it is not repaired automatically.
    #[error("corrupt data in {} at offset {offset}: {reason}", path.display())]
    Corrupt { path: PathBuf, offset: u64, reason: String },

    #[error("the store hit a write error and must be reopened")]
    Poisoned,

    #[error("record too large: {0} bytes")]
    RecordTooLarge(usize),

    #[error("operation rejected: {0}")]
    InvalidOp(String),

    #[error(transparent)]
    Codec(#[from] CodecError),

    #[error("index: {0}")]
    Index(String),
}

impl StoreError {
    pub(crate) fn corrupt(path: &Path, offset: u64, reason: impl Into<String>) -> Self {
        StoreError::Corrupt { path: path.to_owned(), offset, reason: reason.into() }
    }

    /// `true` if the error is "out of disk space".
    pub fn is_storage_full(&self) -> bool {
        matches!(self, StoreError::Io { source, .. } if source.kind() == io::ErrorKind::StorageFull)
    }
}

/// Adds the path to an I/O error.
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
