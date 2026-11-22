//! An archive of a directory, compressed and sealed (ADR 0017): what a
//! server's backups are, and what a repository is sent as when it is
//! pushed to a server (ADR 0018).
//!
//! `KITOBAK1`, four random bytes, then chunks of at most a mebibyte, each
//! `[length: u32][sealed]`, sealed with ChaCha20-Poly1305 under a key of 32
//! bytes, the chunk's number as the nonce and `more` or `end` as what is
//! authenticated with it: a chunk taken out, moved or cut off is refused.
//! Inside, zstd of the files: for each, a line of JSON (`path`, `mode`,
//! `len`) and its bytes.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"KITOBAK1";
pub const CHUNK: usize = 1 << 20;

/// A new key, for an archive that only travels (a push).
pub fn random_key() -> Result<[u8; 32], String> {
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| format!("no randomness to make a key from: {e}"))?;
    Ok(key)
}

fn sealer(key: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).expect("a key of 32 bytes"))
}

fn nonce(prefix: [u8; 4], n: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[..4].copy_from_slice(&prefix);
    bytes[4..].copy_from_slice(&n.to_be_bytes());
    Nonce::assume_unique_for_key(bytes)
}

/// Seals what is written into it, a chunk at a time.
struct Sealing<W: Write> {
    out: W,
    key: LessSafeKey,
    prefix: [u8; 4],
    n: u64,
    buffer: Vec<u8>,
}

impl<W: Write> Sealing<W> {
    fn new(mut out: W, key: &[u8; 32]) -> std::io::Result<Self> {
        let mut prefix = [0u8; 4];
        getrandom::getrandom(&mut prefix).map_err(|e| std::io::Error::other(e.to_string()))?;
        out.write_all(MAGIC)?;
        out.write_all(&prefix)?;
        Ok(Sealing { out, key: sealer(key), prefix, n: 0, buffer: Vec::with_capacity(CHUNK) })
    }

    fn seal(&mut self, last: bool) -> std::io::Result<()> {
        let mut chunk = std::mem::take(&mut self.buffer);
        let aad = if last { &b"end"[..] } else { &b"more"[..] };
        self.key
            .seal_in_place_append_tag(nonce(self.prefix, self.n), Aad::from(aad), &mut chunk)
            .map_err(|_| std::io::Error::other("sealing a chunk"))?;
        self.n += 1;
        self.out.write_all(&(chunk.len() as u32).to_be_bytes())?;
        self.out.write_all(&chunk)
    }

    fn finish(mut self) -> std::io::Result<W> {
        self.seal(true)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for Sealing<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let room = CHUNK - self.buffer.len();
        let taken = room.min(data.len());
        self.buffer.extend_from_slice(&data[..taken]);
        if self.buffer.len() == CHUNK {
            self.seal(false)?;
        }
        Ok(taken)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Opens what `Sealing` sealed, refusing what was changed, moved or cut.
struct Opening<R: Read> {
    input: R,
    key: LessSafeKey,
    prefix: [u8; 4],
    n: u64,
    chunk: Vec<u8>,
    at: usize,
    ended: bool,
}

impl<R: Read> Opening<R> {
    fn new(mut input: R, key: &[u8; 32]) -> Result<Self, String> {
        let mut head = [0u8; 12];
        input.read_exact(&mut head).map_err(|_| "not an archive of Kito".to_owned())?;
        if &head[..8] != MAGIC {
            return Err("not an archive of Kito".into());
        }
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&head[8..]);
        Ok(Opening { input, key: sealer(key), prefix, n: 0, chunk: Vec::new(), at: 0, ended: false })
    }

    fn next_chunk(&mut self) -> std::io::Result<()> {
        let mut length = [0u8; 4];
        self.input.read_exact(&mut length).map_err(|_| std::io::Error::other("the archive is cut short"))?;
        let mut sealed = vec![0u8; u32::from_be_bytes(length) as usize];
        self.input.read_exact(&mut sealed).map_err(|_| std::io::Error::other("the archive is cut short"))?;
        for (aad, last) in [(&b"more"[..], false), (&b"end"[..], true)] {
            let mut trying = sealed.clone();
            if let Ok(plain) = self.key.open_in_place(nonce(self.prefix, self.n), Aad::from(aad), &mut trying)
            {
                self.chunk = plain.to_vec();
                self.at = 0;
                self.n += 1;
                self.ended = last;
                return Ok(());
            }
        }
        Err(std::io::Error::other("the archive was changed, or this is not its key"))
    }
}

impl<R: Read> Read for Opening<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.at == self.chunk.len() {
            if self.ended {
                return Ok(0);
            }
            self.next_chunk()?;
        }
        let n = out.len().min(self.chunk.len() - self.at);
        out[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// The files under `root`, from it, but those `left_out` says.
pub fn collect(root: &Path, left_out: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    fn walk(root: &Path, relative: &Path, left_out: &dyn Fn(&Path) -> bool, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(root.join(relative)) else { return };
        for entry in entries.flatten() {
            let path = relative.join(entry.file_name());
            if left_out(&path) {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk(root, &path, left_out, found),
                Ok(kind) if kind.is_file() => found.push(path),
                _ => {}
            }
        }
    }
    let mut found = Vec::new();
    walk(root, Path::new(""), left_out, &mut found);
    found
}

/// Writes the files (from `root`) into an archive at `out`; answers how
/// many went in and their bytes.
pub fn write(root: &Path, files: &[PathBuf], out: &Path, key: &[u8; 32]) -> Result<(usize, u64), String> {
    let said = |e: std::io::Error| format!("{}: {e}", out.display());
    let staged = out.with_extension("new");
    let file = std::fs::File::create(&staged).map_err(said)?;
    let sealing = Sealing::new(std::io::BufWriter::new(file), key).map_err(said)?;
    let mut packed = zstd::Encoder::new(sealing, 3).map_err(said)?;
    let (mut count, mut bytes) = (0usize, 0u64);
    for relative in files {
        let path = root.join(relative);
        // A file that went away meanwhile is not in it.
        let Ok(content) = std::fs::read(&path) else { continue };
        #[cfg(unix)]
        let mode = std::fs::metadata(&path)
            .map(|meta| std::os::unix::fs::PermissionsExt::mode(&meta.permissions()))
            .unwrap_or(0o644);
        #[cfg(not(unix))]
        let mode = 0o644u32;
        let head = json!({ "path": relative.to_string_lossy().replace('\\', "/"), "mode": mode, "len": content.len() });
        packed.write_all(format!("{head}\n").as_bytes()).map_err(said)?;
        packed.write_all(&content).map_err(said)?;
        count += 1;
        bytes += content.len() as u64;
    }
    let sealing = packed.finish().map_err(said)?;
    sealing
        .finish()
        .map_err(said)?
        .into_inner()
        .map_err(|e| said(e.into_error()))?
        .sync_all()
        .map_err(said)?;
    std::fs::rename(&staged, out).map_err(said)?;
    Ok((count, bytes))
}

/// Puts the files of an archive in `to`, which must be empty or not there.
/// Answers how many files.
pub fn read(archive: impl Read, key: &[u8; 32], to: &Path) -> Result<usize, String> {
    if to.exists() && std::fs::read_dir(to).map(|mut entries| entries.next().is_some()).unwrap_or(true) {
        return Err(format!("{} is not empty: an archive is put back into an empty directory", to.display()));
    }
    let opening = Opening::new(std::io::BufReader::new(archive), key)?;
    let mut unpacked = std::io::BufReader::new(zstd::Decoder::new(opening).map_err(|e| e.to_string())?);
    let mut count = 0;
    loop {
        let mut line = String::new();
        let n = std::io::BufRead::read_line(&mut unpacked, &mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let head: Value =
            serde_json::from_str(&line).map_err(|_| "the archive's contents are not in order".to_owned())?;
        let relative = head["path"].as_str().unwrap_or_default();
        // Nothing outside the directory it is put back in.
        let path = Path::new(relative);
        if relative.is_empty()
            || path.is_absolute()
            || path.components().any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(format!("the archive names a path outside its directory: {relative}"));
        }
        let mut content = vec![0u8; head["len"].as_u64().unwrap_or(0) as usize];
        unpacked.read_exact(&mut content).map_err(|e| e.to_string())?;
        let target = to.join(path);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&target, &content).map_err(|e| format!("{}: {e}", target.display()))?;
        #[cfg(unix)]
        if let Some(mode) = head["mode"].as_u64() {
            let _ = std::fs::set_permissions(
                &target,
                std::os::unix::fs::PermissionsExt::from_mode(mode as u32 & 0o777),
            );
        }
        count += 1;
    }
    Ok(count)
}
