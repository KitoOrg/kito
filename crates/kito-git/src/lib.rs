//! `kito-git`: the bridge to git.
//!
//! * [`export`]: every checkpoint of the main lane becomes a git commit, so
//!   GitHub and CI work without changes. SHA-1 appears only here.
//! * [`import`]: the history of a git repository becomes ops and checkpoints.

mod export;
mod import;

pub use export::{export, ExportOptions, Exported};
pub use import::{import, Imported};

use kito_core::{CheckpointId, CodecError, OpId, TreeError};
use kito_store::StoreError;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git: {0}")]
    Git(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("{} is not a git repository", .0.display())]
    NotARepository(PathBuf),
    #[error(
        "the git branch {branch} has advanced outside Kito (it points to {found}, expected {expected}): \
         commits made directly in git must be imported first"
    )]
    Diverged { branch: String, expected: String, found: String },
    #[error("{0}: the op {1} it points to is not on the main lane")]
    HeadNotOnMain(CheckpointId, OpId),
    #[error("the store already has history: import only works on an empty store")]
    NotEmpty,
    #[error("blob {0} is missing from the store")]
    MissingBlob(kito_core::BlobId),
}

/// Converts any gix error.
pub(crate) fn git_err(e: impl std::fmt::Display) -> GitError {
    GitError::Git(e.to_string())
}

/// Name and email configured in git (`user.name`, `user.email`): first those
/// of the repository in `root`, if there is one, then the user's global ones.
pub fn git_identity(root: &std::path::Path) -> Option<(String, Option<String>)> {
    let clean =
        |v: Option<gix::bstr::BString>| v.map(|s| s.to_string().trim().to_owned()).filter(|s| !s.is_empty());
    let from_repo = gix::open(root).ok().and_then(|repo| {
        let config = repo.config_snapshot();
        clean(config.string("user.name")).map(|name| (name, clean(config.string("user.email"))))
    });
    from_repo.or_else(|| {
        let config = gix::config::File::from_globals().ok()?;
        clean(config.string("user.name")).map(|name| (name, clean(config.string("user.email"))))
    })
}

pub(crate) fn oid20(id: gix::ObjectId) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(id.as_slice());
    out
}

pub(crate) fn object_id(bytes: &[u8; 20]) -> gix::ObjectId {
    gix::ObjectId::from_bytes_or_panic(bytes)
}
