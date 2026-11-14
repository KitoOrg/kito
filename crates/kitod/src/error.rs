//! Daemon errors.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] kito_store::StoreError),
    #[error(transparent)]
    Tree(#[from] kito_core::TreeError),
    #[error(transparent)]
    Symbol(#[from] kito_core::SymbolError),
    #[error(transparent)]
    Codec(#[from] kito_core::CodecError),
    #[error(transparent)]
    Git(#[from] kito_git::GitError),
    #[error("configuration: {0}")]
    Config(String),
    #[error("I/O on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("nothing to checkpoint: no change has been captured yet")]
    NothingToCheckpoint,
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
        let path = path.into();
        move |source| Error::Io { path, source }
    }
}
