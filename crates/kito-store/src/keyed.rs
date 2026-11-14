//! Append-only log of records that are updated by appending: the last
//! record with a given key wins. Lanes and conflicts are kept this way.

use crate::error::{Result, StoreError};
use crate::fs::FileSystem;
use crate::logfile::{LogFile, LogRecovery};
use crate::record::LogKind;
use kito_core::codec;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) struct KeyedLog<K, V> {
    file: LogFile,
    by_key: BTreeMap<K, V>,
    key: fn(&V) -> K,
}

impl<K: Ord + Copy, V: Serialize + DeserializeOwned + Clone> KeyedLog<K, V> {
    /// Opens the log, creating it if it is missing. `what` names a record
    /// in error messages.
    pub(crate) fn open(
        path: &Path,
        kind: LogKind,
        what: &'static str,
        key: fn(&V) -> K,
        fs: &dyn FileSystem,
        lost_found: &Path,
        floor: Option<u64>,
    ) -> Result<(KeyedLog<K, V>, LogRecovery)> {
        let (file, recovery) = if path.exists() {
            LogFile::recover(fs, path, kind, 0, lost_found, floor)?
        } else {
            if let Some(floor) = floor.filter(|&f| f > crate::record::HEADER_LEN as u64) {
                return Err(StoreError::corrupt(
                    path,
                    0,
                    format!("the file is missing but {floor} bytes are recorded as confirmed: to accept the loss, reopen the store in repair mode"),
                ));
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|source| StoreError::Io { path: dir.to_owned(), source })?;
            }
            (LogFile::create(fs, path, kind, 0)?, LogRecovery::default())
        };
        let mut by_key = BTreeMap::new();
        file.scan(0, &mut |off, payload, _| {
            let value: V = codec::from_slice(payload)
                .map_err(|e| StoreError::corrupt(path, off, format!("unreadable {what}: {e}")))?;
            by_key.insert(key(&value), value);
            Ok(())
        })?;
        Ok((KeyedLog { file, by_key, key }, recovery))
    }

    /// Records (or updates) a value durably.
    pub(crate) fn put(&mut self, value: &V) -> Result<()> {
        let bytes = codec::to_vec(value)?;
        self.file.append(&[&bytes])?;
        self.file.sync()?;
        self.by_key.insert((self.key)(value), value.clone());
        Ok(())
    }

    pub(crate) fn get(&self, key: K) -> Option<&V> {
        self.by_key.get(&key)
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &V> {
        self.by_key.values()
    }

    pub(crate) fn last_key(&self) -> Option<K> {
        self.by_key.keys().next_back().copied()
    }

    /// Length of the file: everything it contains is durable.
    pub(crate) fn len(&self) -> u64 {
        self.file.len()
    }
}
