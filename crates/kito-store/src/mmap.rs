//! Lettura dei file di log tramite `mmap`.
//!
//! È l'unico modulo del progetto con codice `unsafe`.

use memmap2::{Mmap, MmapOptions};
use std::fs::File;
use std::io;

/// Mappatura in sola lettura di un prefisso di un file di log.
pub(crate) struct MappedFile {
    /// `None` per un prefisso vuoto (una mappatura lunga zero non è valida).
    map: Option<Mmap>,
}

impl MappedFile {
    /// Mappa in sola lettura i primi `len` byte di `file`.
    ///
    /// Chi chiama deve rispettare l'invariante dei log append-only: i byte
    /// `0..len` sono già stati scritti e non verranno mai più modificati né
    /// rimossi finché la mappatura esiste.
    pub(crate) fn map_prefix(file: &File, len: u64) -> io::Result<MappedFile> {
        let actual = file.metadata()?.len();
        if actual < len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("file di {actual} byte, più corto dei {len} da mappare"),
            ));
        }
        if len == 0 {
            return Ok(MappedFile { map: None });
        }
        let len = usize::try_from(len).map_err(|_| io::Error::other("file troppo grande per mmap"))?;
        // SAFETY: mappare un file è unsafe perché, se il file viene troncato
        // sotto la lunghezza mappata o modificato mentre la mappatura è viva,
        // leggere la slice è comportamento indefinito (SIGBUS, o dati che
        // cambiano sotto un `&[u8]`). Qui non può accadere perché:
        //   1. il controllo sopra garantisce che il file contiene almeno `len` byte;
        //   2. i file di log sono append-only: i byte già scritti non vengono
        //      mai riscritti (i file sono aperti in scrittura solo con O_APPEND);
        //   3. l'unico scrittore è il processo che detiene il lock esclusivo
        //      dello store, e tronca un file solo (a) durante il recupero,
        //      dopo aver rilasciato la propria mappatura e prima di crearne
        //      altre, oppure (b) per annullare una scrittura parziale fallita,
        //      tornando alla lunghezza `written`, che non è mai inferiore a
        //      quella di una mappatura esistente (si mappa al più `written`);
        //   4. la compattazione rimuove file interi con unlink, che lascia
        //      valide le mappature esistenti.
        // Il test `mapping_is_stable_while_the_file_grows` esercita il caso 2,
        // `refuses_to_map_beyond_end_of_file` il caso 1.
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

        // Il file cresce di molte pagine: la vecchia mappatura non cambia.
        let more = vec![0xabu8; 1 << 20];
        w.write_all(&more).unwrap();
        w.sync_data().unwrap();
        assert_eq!(old.len(), first.len() as u64);
        assert_eq!(old.bytes(), &first[..]);

        // Una nuova mappatura vede anche i dati aggiunti, e il prefisso coincide.
        let new = MappedFile::map_prefix(&r, (first.len() + more.len()) as u64).unwrap();
        assert_eq!(&new.bytes()[..first.len()], &first[..]);
        assert_eq!(&new.bytes()[first.len()..], &more[..]);

        // Si può mappare anche un prefisso che non finisce su un confine di pagina.
        let partial = MappedFile::map_prefix(&r, 4097).unwrap();
        assert_eq!(partial.bytes(), &first[..4097]);

        // La mappatura sopravvive alla rimozione del file (caso della compattazione).
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
