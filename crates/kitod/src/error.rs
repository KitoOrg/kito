//! Errori del daemon.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] kito_store::StoreError),
    #[error(transparent)]
    Tree(#[from] kito_core::TreeError),
    #[error(transparent)]
    Codec(#[from] kito_core::CodecError),
    #[error(transparent)]
    Git(#[from] kito_git::GitError),
    #[error("configurazione: {0}")]
    Config(String),
    #[error("I/O su {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("niente da salvare: nessuna modifica è stata ancora catturata")]
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
