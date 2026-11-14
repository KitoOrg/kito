//! Log append-only degli intent (`.kito/intents`): il "perché" delle op.

use crate::error::{Result, StoreError};
use crate::fs::FileSystem;
use crate::logfile::{LogFile, LogRecovery};
use crate::record::LogKind;
use kito_core::{codec, Intent, IntentId};
use std::collections::HashMap;
use std::path::Path;

pub(crate) struct IntentLog {
    file: LogFile,
    by_id: HashMap<IntentId, Intent>,
}

impl IntentLog {
    pub(crate) fn open(
        path: &Path,
        fs: &dyn FileSystem,
        lost_found: &Path,
        floor: Option<u64>,
    ) -> Result<(IntentLog, LogRecovery)> {
        let (file, recovery) = if path.exists() {
            LogFile::recover(fs, path, LogKind::Intents, 0, lost_found, floor)?
        } else {
            if let Some(floor) = floor.filter(|&f| f > crate::record::HEADER_LEN as u64) {
                return Err(StoreError::corrupt(
                    path,
                    0,
                    format!("il file manca ma risultano confermati {floor} byte: per accettare la perdita riaprire lo store in modalità riparazione"),
                ));
            }
            (LogFile::create(fs, path, LogKind::Intents, 0)?, LogRecovery::default())
        };
        let mut by_id = HashMap::new();
        file.scan(0, &mut |off, payload, _| {
            let intent: Intent = codec::from_slice(payload)
                .map_err(|e| StoreError::corrupt(path, off, format!("intent illeggibile: {e}")))?;
            by_id.insert(intent.id, intent);
            Ok(())
        })?;
        Ok((IntentLog { file, by_id }, recovery))
    }

    /// Registra un intent in modo durevole. Uno già presente non viene riscritto.
    pub(crate) fn put(&mut self, intent: &Intent) -> Result<()> {
        if self.by_id.contains_key(&intent.id) {
            return Ok(());
        }
        let bytes = codec::to_vec(intent)?;
        self.file.append(&[&bytes])?;
        self.file.sync()?;
        self.by_id.insert(intent.id, intent.clone());
        Ok(())
    }

    pub(crate) fn get(&self, id: &IntentId) -> Option<&Intent> {
        self.by_id.get(id)
    }

    pub(crate) fn len(&self) -> u64 {
        self.file.len()
    }
}
