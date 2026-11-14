//! Accesso in scrittura ai file, dietro un trait: permette di iniettare
//! guasti (disco pieno, fsync fallito) nei test senza toccare il codice dello
//! store.

use std::fmt::Debug;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Un file aperto in sola aggiunta.
pub trait RawFile: Send + Sync {
    /// Accoda tutti i byte in fondo al file. In caso di errore può averne
    /// scritto solo una parte.
    fn append(&mut self, buf: &[u8]) -> io::Result<()>;
    /// Rende durevoli i dati scritti (`fdatasync`).
    fn sync(&mut self) -> io::Result<()>;
    /// Tronca il file a `len` byte.
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

/// Le operazioni di scrittura su file system usate dallo store.
pub trait FileSystem: Send + Sync + Debug {
    /// Apre un file esistente in sola aggiunta.
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn RawFile>>;

    /// Crea (o sostituisce) `path` con `content` in modo atomico:
    /// scrittura su file temporaneo, fsync, rename, fsync della cartella.
    fn create_atomic(&self, path: &Path, content: &[u8]) -> io::Result<()>;
}

/// Il file system reale.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFs;

struct StdFile(File);

impl RawFile for StdFile {
    fn append(&mut self, buf: &[u8]) -> io::Result<()> {
        self.0.write_all(buf)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
}

impl FileSystem for StdFs {
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn RawFile>> {
        Ok(Box::new(StdFile(OpenOptions::new().append(true).open(path)?)))
    }

    fn create_atomic(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        atomic_write(path, content)
    }
}

/// Suffisso dei file temporanei delle scritture atomiche.
pub(crate) const TMP_SUFFIX: &str = ".tmp";

/// Scrittura atomica: file temporaneo, fsync, rename, fsync della cartella.
/// Dopo un crash il file ha il contenuto vecchio oppure quello nuovo, intero.
pub fn atomic_write(path: &Path, content: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(TMP_SUFFIX);
    let tmp = Path::new(&tmp);
    let result = (|| {
        let mut f = File::create(tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        drop(f);
        fs::rename(tmp, path)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// Rende durevoli le voci di una cartella (creazioni, rename, rimozioni).
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Rimuove i file temporanei lasciati da scritture atomiche interrotte.
pub(crate) fn remove_stray_tmp(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().ends_with(TMP_SUFFIX) && entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state");
        atomic_write(&p, b"uno").unwrap();
        atomic_write(&p, b"due").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"due");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn stray_tmp_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("00000001.seg.tmp"), b"x").unwrap();
        fs::write(dir.path().join("00000000.seg"), b"y").unwrap();
        remove_stray_tmp(dir.path()).unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["00000000.seg"]);
    }
}
