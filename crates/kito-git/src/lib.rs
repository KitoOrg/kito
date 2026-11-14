//! `kito-git`: il ponte con git.
//!
//! * [`export`]: ogni checkpoint della linea principale diventa un commit
//!   git, così GitHub e la CI funzionano senza modifiche. SHA-1 compare solo qui.
//! * [`import`]: la storia di un repository git diventa op e checkpoint.

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
    #[error("{} non è un repository git", .0.display())]
    NotARepository(PathBuf),
    #[error(
        "il ramo git {branch} è avanzato fuori da Kito (punta a {found}, atteso {expected}): \
         i commit fatti direttamente in git vanno prima importati"
    )]
    Diverged { branch: String, expected: String, found: String },
    #[error("{0}: l'op {1} a cui punta non è sulla linea principale")]
    HeadNotOnMain(CheckpointId, OpId),
    #[error("lo store contiene già una storia: l'import si fa solo in uno store vuoto")]
    NotEmpty,
    #[error("blob {0} assente dallo store")]
    MissingBlob(kito_core::BlobId),
}

/// Converte un qualsiasi errore di gix.
pub(crate) fn git_err(e: impl std::fmt::Display) -> GitError {
    GitError::Git(e.to_string())
}

/// Nome ed email configurati in git (`user.name`, `user.email`): prima quelli
/// del repository in `root`, se c'è, poi quelli globali dell'utente.
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
