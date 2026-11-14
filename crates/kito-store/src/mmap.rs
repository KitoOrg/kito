//! Reading log files through `mmap`.
//!
//! It is the only module of the project with `unsafe` code.

use memmap2::{Mmap, MmapOptions};
use std::fs::File;
use std::io;

/// Read-only mapping of a prefix of a log file.
pub(crate) struct MappedFile {
    /// `None` for an empty prefix (a zero-length mapping is not valid).
    map: Option<Mmap>,
}

impl MappedFile {
    /// Maps the first `len` bytes of `file` read-only.
    ///
    /// The caller must uphold the invariant of append-only logs: the bytes
    /// `0..len` have already been written and will never again be modified
    /// or removed for as long as the mapping exists.
    pub(crate) fn map_prefix(file: &File, len: u64) -> io::Result<MappedFile> {
        let actual = file.metadata()?.len();
        if actual < len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("file of {actual} bytes, shorter than the {len} to map"),
            ));
        }
        if len == 0 {
            return Ok(MappedFile { map: None });
        }
        let len = usize::try_from(len).map_err(|_| io::Error::other("file too large for mmap"))?;
        // SAFETY: mapping a file is unsafe because, if the file is truncated
        // below the mapped length or modified while the mapping is alive,
        // reading the slice is undefined behavior (SIGBUS, or data changing
        // underneath a `&[u8]`). It cannot happen here because:
        //   1. the check above guarantees that the file holds at least `len` bytes;
        //   2. log files are append-only: bytes already written are never
        //      rewritten (files are opened for writing only with O_APPEND);
        //   3. the only writer is the process that holds the store's exclusive
        //      lock, and it truncates a file only (a) during recovery, after
        //      releasing its own mapping and before creating any others, or
        //      (b) to undo a failed partial write, going back to the length
        //      `written`, which is never less than that of an existing
        //      mapping (at most `written` bytes are mapped);
        //   4. compaction removes whole files with unlink, which leaves the
        //      existing mappings valid.
        // The test `mapping_is_stable_while_the_file_grows` exercises case 2,
        // `refuses_to_map_beyond_end_of_file` case 1.
        let map = unsafe { MmapOptions::new().len(len).map(file)? };
        Ok(MappedFile { map: Some(map) })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.map.as_deref().unwrap_or(&[])
    }

    pub(crate) fn len(&self) -> u64 {
        self.bytes().len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

    #[test]
    fn mapping_is_stable_while_the_file_grows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let mut w = OpenOptions::new().create(true).append(true).open(&path).unwrap();
        let first: Vec<u8> = (0..10_000u32).flat_map(|i| i.to_le_bytes()).collect();
        w.write_all(&first).unwrap();

        let r = File::open(&path).unwrap();
        let old = MappedFile::map_prefix(&r, first.len() as u64).unwrap();
        assert_eq!(old.bytes(), &first[..]);

        // The file grows by many pages: the old mapping does not change.
        let more = vec![0xabu8; 1 << 20];
        w.write_all(&more).unwrap();
        w.sync_data().unwrap();
        assert_eq!(old.len(), first.len() as u64);
        assert_eq!(old.bytes(), &first[..]);

        // A new mapping also sees the appended data, and the prefix matches.
        let new = MappedFile::map_prefix(&r, (first.len() + more.len()) as u64).unwrap();
        assert_eq!(&new.bytes()[..first.len()], &first[..]);
        assert_eq!(&new.bytes()[first.len()..], &more[..]);

        // A prefix that does not end on a page boundary can be mapped too.
        let partial = MappedFile::map_prefix(&r, 4097).unwrap();
        assert_eq!(partial.bytes(), &first[..4097]);

        // The mapping survives the removal of the file (the compaction case).
        std::fs::remove_file(&path).unwrap();
        assert_eq!(old.bytes(), &first[..]);
    }

    #[test]
    fn refuses_to_map_beyond_end_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"12345").unwrap();
        let r = File::open(&path).unwrap();
        assert!(MappedFile::map_prefix(&r, 6).is_err());
        assert_eq!(MappedFile::map_prefix(&r, 5).unwrap().bytes(), b"12345");
    }

    #[test]
    fn empty_prefix_needs_no_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"").unwrap();
        let m = MappedFile::map_prefix(&File::open(&path).unwrap(), 0).unwrap();
        assert!(m.bytes().is_empty());
        assert_eq!(m.len(), 0);
    }
}
