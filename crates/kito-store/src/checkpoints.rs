//! Append-only log of the checkpoints (`.kito/checkpoints`).
//!
//! A checkpoint is updated (for example with the git commit after the export)
//! by appending a new record with the same ID: the last one wins.

use crate::error::{Result, StoreError};
use crate::fs::FileSystem;
use crate::logfile::{LogFile, LogRecovery};
use crate::record::LogKind;
use kito_core::{codec, Checkpoint, CheckpointId};
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) struct CheckpointLog {
    file: LogFile,
    by_id: BTreeMap<CheckpointId, Checkpoint>,
}

impl CheckpointLog {
    pub(crate) fn open(
        path: &Path,
        fs: &dyn FileSystem,
        lost_found: &Path,
        floor: Option<u64>,
    ) -> Result<(CheckpointLog, LogRecovery)> {
        let (file, recovery) = if path.exists() {
            LogFile::recover(fs, path, LogKind::Checkpoints, 0, lost_found, floor)?
        } else {
            if let Some(floor) = floor.filter(|&f| f > crate::record::HEADER_LEN as u64) {
                return Err(StoreError::corrupt(
                    path,
                    0,
                    format!("the file is missing but {floor} bytes are recorded as confirmed: to accept the loss, reopen the store in repair mode"),
                ));
            }
            (LogFile::create(fs, path, LogKind::Checkpoints, 0)?, LogRecovery::default())
        };
        let mut by_id = BTreeMap::new();
        file.scan(0, &mut |off, payload, _| {
            let ck: Checkpoint = codec::from_slice(payload)
                .map_err(|e| StoreError::corrupt(path, off, format!("unreadable checkpoint: {e}")))?;
            by_id.insert(ck.id, ck);
            Ok(())
        })?;
        Ok((CheckpointLog { file, by_id }, recovery))
    }

    /// Records (or updates) a checkpoint durably.
    pub(crate) fn put(&mut self, ck: &Checkpoint) -> Result<()> {
        let bytes = codec::to_vec(ck)?;
        self.file.append(&[&bytes])?;
        self.file.sync()?;
        self.by_id.insert(ck.id, ck.clone());
        Ok(())
    }

    pub(crate) fn get(&self, id: CheckpointId) -> Option<&Checkpoint> {
        self.by_id.get(&id)
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &Checkpoint> {
        self.by_id.values()
    }

    /// Length of the file: everything it contains is durable.
    pub(crate) fn len(&self) -> u64 {
        self.file.len()
    }

    pub(crate) fn next_id(&self) -> CheckpointId {
        CheckpointId(self.by_id.keys().next_back().map_or(1, |id| id.0 + 1))
    }
}
