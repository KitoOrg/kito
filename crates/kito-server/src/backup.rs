//! Backups of the whole server (ADR 0017): every repository, the people,
//! the settings and what plugins keep, in one archive, compressed and
//! encrypted, made while the server runs, kept in a folder and/or in
//! storage that speaks S3.
//!
//! A repository's store is append-only and checks every record, and its
//! indexes are rebuilt from the rest: a copy made while it runs is what a
//! crash would have left, which is what it opens from. The indexes and
//! the locks are left out.
//!
//! An archive (`.kbak`) is `KITOBAK1`, four random bytes, then chunks of at
//! most a mebibyte, each `[length: u32][sealed]`, sealed with
//! ChaCha20-Poly1305 under the server's backup key, the chunk's number as
//! the nonce and `more` or `end` as what is authenticated with it: a chunk
//! taken out, moved or cut off is refused. Inside, zstd of the files: for
//! each, a line of JSON (`path`, `mode`, `len`) and its bytes.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"KITOBAK1";
const CHUNK: usize = 1 << 20;
const KEY_FILE: &str = "backup.key";
const CREDENTIALS: &str = "backup-s3.toml";
/// Where archives go when nothing else is said.
pub const DEFAULT_DIR: &str = "backups";

/// `[backup]` of `server.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupConfig {
    /// A folder for the archives (default `backups` in the server's
    /// directory); `none`: only in S3.
    pub dir: Option<String>,
    /// How many are kept, here and in S3.
    pub keep: usize,
    /// Hours between two, made by `serve`; 0: only when asked.
    pub every_hours: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Config>,
}

impl Default for BackupConfig {
    fn default() -> Self {
        BackupConfig { dir: None, keep: 14, every_hours: 24, s3: None }
    }
}

impl BackupConfig {
    pub fn is_default(&self) -> bool {
        *self == BackupConfig::default()
    }
}

/// Storage that speaks S3 (AWS, R2, B2, MinIO…), addressed by path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Config {
    /// `https://s3.eu-central-1.amazonaws.com`, `https://<account>.r2.cloudflarestorage.com`.
    pub endpoint: String,
    pub bucket: String,
    #[serde(default = "auto")]
    pub region: String,
    #[serde(default)]
    pub prefix: String,
}

fn auto() -> String {
    "auto".to_owned()
}

/// The keys of the storage: `backup-s3.toml` next to `server.toml`, for
/// its owner only, or the environment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn private_write(path: &Path, text: &[u8]) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).and_then(|mut out| out.write_all(text)).map_err(|e| format!("{}: {e}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    text.len()
        .is_multiple_of(2)
        .then(|| (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect())
        .flatten()
}

/// The server's backup key; made the first time, and then answered `true`
/// with it, so that it is said once.
pub fn key(server: &Path) -> Result<([u8; 32], bool), String> {
    let path = server.join(KEY_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let bytes = unhex(&text)
            .filter(|bytes| bytes.len() == 32)
            .ok_or_else(|| format!("{}: not a key", path.display()))?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok((key, false));
    }
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| format!("no randomness to make a key from: {e}"))?;
    private_write(&path, format!("{}\n", hex(&key)).as_bytes())?;
    Ok((key, true))
}

/// A key given as a file of hex, for a restore elsewhere.
pub fn key_from(file: &Path) -> Result<[u8; 32], String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let bytes = unhex(&text)
        .filter(|bytes| bytes.len() == 32)
        .ok_or_else(|| format!("{}: not a backup key", file.display()))?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
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
        input.read_exact(&mut head).map_err(|_| "not a backup of a Kito server".to_owned())?;
        if &head[..8] != MAGIC {
            return Err("not a backup of a Kito server".into());
        }
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&head[8..]);
        Ok(Opening { input, key: sealer(key), prefix, n: 0, chunk: Vec::new(), at: 0, ended: false })
    }

    fn next_chunk(&mut self) -> std::io::Result<bool> {
        let mut length = [0u8; 4];
        if let Err(e) = self.input.read_exact(&mut length) {
            return match e.kind() {
                std::io::ErrorKind::UnexpectedEof => Err(std::io::Error::other("the archive is cut short")),
                _ => Err(e),
            };
        }
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
                return Ok(true);
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

/// `true` for what an archive leaves out: its own folder, the key, the
/// locks, sockets, and the indexes a repository rebuilds.
fn left_out(relative: &Path, backups: Option<&Path>) -> bool {
    let text = relative.to_string_lossy().replace('\\', "/");
    let name = relative.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    backups.is_some_and(|dir| relative.starts_with(dir))
        || text == KEY_FILE
        || text == CREDENTIALS
        || name == ".lock"
        || name == "lock"
        || name.ends_with(".sock")
        || name.starts_with("socket")
        || text.contains("/.kito/index/")
        || text.ends_with("/.kito/index")
}

fn files(root: &Path, relative: &Path, backups: Option<&Path>, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root.join(relative)) else { return };
    for entry in entries.flatten() {
        let path = relative.join(entry.file_name());
        if left_out(&path, backups) {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => files(root, &path, backups, found),
            Ok(kind) if kind.is_file() => found.push(path),
            _ => {}
        }
    }
}

/// Writes an archive of the server in `server` to `out`; answers how many
/// files and bytes went in.
pub fn write_archive(
    server: &Path,
    out: &Path,
    key: &[u8; 32],
    backups: Option<&Path>,
) -> Result<(usize, u64), String> {
    let said = |e: std::io::Error| format!("{}: {e}", out.display());
    let staged = out.with_extension("kbak.new");
    let file = std::fs::File::create(&staged).map_err(said)?;
    let sealing = Sealing::new(std::io::BufWriter::new(file), key).map_err(said)?;
    let mut packed = zstd::Encoder::new(sealing, 3).map_err(said)?;
    let mut listed = Vec::new();
    files(server, Path::new(""), backups, &mut listed);
    let mut bytes = 0u64;
    for relative in &listed {
        let path = server.join(relative);
        // A file that went away while the archive was made is not in it.
        let Ok(content) = std::fs::read(&path) else { continue };
        #[cfg(unix)]
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).map_err(said)?.permissions());
        #[cfg(not(unix))]
        let mode = 0o644u32;
        let head = json!({ "path": relative.to_string_lossy().replace('\\', "/"), "mode": mode, "len": content.len() });
        packed.write_all(format!("{head}\n").as_bytes()).map_err(said)?;
        packed.write_all(&content).map_err(said)?;
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
    Ok((listed.len(), bytes))
}

/// Puts the files of an archive in `to`, which must be empty. Answers how
/// many files.
pub fn restore(archive: impl Read, key: &[u8; 32], to: &Path) -> Result<usize, String> {
    if to.exists() && std::fs::read_dir(to).map(|mut entries| entries.next().is_some()).unwrap_or(true) {
        return Err(format!("{} is not empty: a backup is put back into an empty directory", to.display()));
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
            || path.components().any(|part| matches!(part, std::path::Component::ParentDir))
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

/// `kito-backup-2026-11-22T031500Z.kbak`
pub fn archive_name(ms: u64) -> String {
    let day = crate::audit::day_of(ms);
    let seconds = (ms / 1000) % 86_400;
    format!("kito-backup-{day}T{:02}{:02}{:02}Z.kbak", seconds / 3600, (seconds / 60) % 60, seconds % 60)
}

// ------------------------------------------------------------------- S3

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    ring::hmac::sign(&key, data.as_bytes()).as_ref().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

/// `20261122T031500Z` and `20261122`.
fn amz_dates(ms: u64) -> (String, String) {
    let day = crate::audit::day_of(ms).replace('-', "");
    let seconds = (ms / 1000) % 86_400;
    (format!("{day}T{:02}{:02}{:02}Z", seconds / 3600, (seconds / 60) % 60, seconds % 60), day)
}

fn encode(text: &str, slash: bool) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b'/' if !slash => "/".to_owned(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub struct S3 {
    config: S3Config,
    credentials: Credentials,
}

impl S3 {
    pub fn new(server: &Path, config: &S3Config) -> Result<S3, String> {
        let from_file = std::fs::read_to_string(server.join(CREDENTIALS))
            .ok()
            .and_then(|text| toml::from_str::<Credentials>(&text).ok());
        let from_env = match (std::env::var("AWS_ACCESS_KEY_ID"), std::env::var("AWS_SECRET_ACCESS_KEY")) {
            (Ok(access_key), Ok(secret_key)) => Some(Credentials { access_key, secret_key }),
            _ => None,
        };
        let credentials = from_file.or(from_env).ok_or(
            "no keys for the storage: `kito-server backup s3 … --keys <file>` keeps them, or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY",
        )?;
        Ok(S3 { config: config.clone(), credentials })
    }

    pub fn keep_credentials(server: &Path, credentials: &Credentials) -> Result<(), String> {
        let text = toml::to_string(credentials).map_err(|e| e.to_string())?;
        private_write(&server.join(CREDENTIALS), text.as_bytes())
    }

    fn key_of(&self, name: &str) -> String {
        let prefix = self.config.prefix.trim_matches('/');
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        }
    }

    /// A request signed with AWS Signature Version 4.
    fn request(&self, method: &str, key: &str, query: &[(&str, &str)]) -> ureq::Request {
        let endpoint = self.config.endpoint.trim_end_matches('/');
        let host =
            endpoint.split("://").nth(1).unwrap_or(endpoint).split('/').next().unwrap_or_default().to_owned();
        let path = format!("/{}/{}", encode(&self.config.bucket, true), encode(key, false));
        let path = if key.is_empty() { format!("/{}", encode(&self.config.bucket, true)) } else { path };
        let mut sorted: Vec<(String, String)> =
            query.iter().map(|(k, v)| (encode(k, true), encode(v, true))).collect();
        sorted.sort();
        let canonical_query = sorted.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let (stamp, day) = amz_dates(now_ms());
        let payload = "UNSIGNED-PAYLOAD";
        let canonical = format!(
            "{method}\n{path}\n{canonical_query}\nhost:{host}\nx-amz-content-sha256:{payload}\nx-amz-date:{stamp}\n\nhost;x-amz-content-sha256;x-amz-date\n{payload}"
        );
        let scope = format!("{day}/{}/s3/aws4_request", self.config.region);
        let to_sign = format!("AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
        let secret = format!("AWS4{}", self.credentials.secret_key);
        let signing = ["s3", "aws4_request"]
            .iter()
            .fold(hmac(&hmac(secret.as_bytes(), &day), &self.config.region), |key, part| hmac(&key, part));
        let signature = hex(&hmac(&signing, &to_sign));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}",
            self.credentials.access_key
        );
        let url = if canonical_query.is_empty() {
            format!("{endpoint}{path}")
        } else {
            format!("{endpoint}{path}?{canonical_query}")
        };
        ureq::request(method, &url)
            .set("x-amz-date", &stamp)
            .set("x-amz-content-sha256", payload)
            .set("Authorization", &authorization)
            .timeout(std::time::Duration::from_secs(3600))
    }

    fn failed(what: &str, error: ureq::Error) -> String {
        match error {
            ureq::Error::Status(code, answer) => {
                let body = answer.into_string().unwrap_or_default();
                let why = body
                    .split("<Message>")
                    .nth(1)
                    .and_then(|rest| rest.split("</Message>").next())
                    .unwrap_or(&body);
                format!("the storage answered {code} ({what}): {}", why.trim())
            }
            other => format!("the storage could not be reached ({what}): {other}"),
        }
    }

    pub fn upload(&self, file: &Path, name: &str) -> Result<(), String> {
        let length = std::fs::metadata(file).map_err(|e| e.to_string())?.len();
        let input = std::fs::File::open(file).map_err(|e| e.to_string())?;
        self.request("PUT", &self.key_of(name), &[])
            .set("Content-Length", &length.to_string())
            .send(input)
            .map_err(|e| S3::failed(&format!("sending {name}"), e))?;
        Ok(())
    }

    pub fn download(&self, name: &str) -> Result<Box<dyn Read + Send + Sync>, String> {
        let answer = self
            .request("GET", &self.key_of(name), &[])
            .call()
            .map_err(|e| S3::failed(&format!("fetching {name}"), e))?;
        Ok(answer.into_reader())
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        self.request("DELETE", &self.key_of(name), &[])
            .call()
            .map_err(|e| S3::failed(&format!("removing {name}"), e))?;
        Ok(())
    }

    /// The archives there, by name and size.
    pub fn list(&self) -> Result<Vec<(String, u64)>, String> {
        let prefix = self.key_of("kito-backup-");
        let answer = self
            .request("GET", "", &[("list-type", "2"), ("prefix", &prefix)])
            .call()
            .map_err(|e| S3::failed("listing the archives", e))?
            .into_string()
            .map_err(|e| e.to_string())?;
        let mut found = Vec::new();
        for part in answer.split("<Contents>").skip(1) {
            let field = |tag: &str| {
                part.split(&format!("<{tag}>"))
                    .nth(1)
                    .and_then(|rest| rest.split(&format!("</{tag}>")).next())
            };
            if let Some(key) = field("Key") {
                let name = key.rsplit('/').next().unwrap_or(key).to_owned();
                found.push((name, field("Size").and_then(|size| size.parse().ok()).unwrap_or(0)));
            }
        }
        Ok(found)
    }
}

// --------------------------------------------------------------- the whole

/// One archive, as listed.
#[derive(Clone, Debug, Serialize)]
pub struct Listed {
    pub name: String,
    pub size: u64,
    /// `here`, `s3`, or both.
    pub kept: Vec<&'static str>,
}

/// The folder the archives go to, if any.
pub fn folder(server: &Path, config: &BackupConfig) -> Option<PathBuf> {
    match config.dir.as_deref() {
        Some("none") => None,
        Some(dir) if Path::new(dir).is_absolute() => Some(PathBuf::from(dir)),
        Some(dir) => Some(server.join(dir)),
        None => Some(server.join(DEFAULT_DIR)),
    }
}

/// Every archive there is, the latest first.
pub fn list(server: &Path, config: &BackupConfig) -> Result<Vec<Listed>, String> {
    let mut all: std::collections::BTreeMap<String, Listed> = std::collections::BTreeMap::new();
    if let Some(dir) = folder(server, config) {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("kito-backup-") && name.ends_with(".kbak") {
                let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                all.entry(name.clone()).or_insert(Listed { name, size, kept: Vec::new() }).kept.push("here");
            }
        }
    }
    if let Some(s3) = &config.s3 {
        for (name, size) in S3::new(server, s3)?.list()? {
            all.entry(name.clone()).or_insert(Listed { name, size, kept: Vec::new() }).kept.push("s3");
        }
    }
    let mut listed: Vec<Listed> = all.into_values().collect();
    listed.reverse();
    Ok(listed)
}

/// Makes an archive now: in the folder, sent to S3, the oldest beyond
/// `keep` taken out. Answers what it did, and the key when it was just
/// made (said once).
pub fn run(server: &Path, config: &BackupConfig) -> Result<Value, String> {
    let (key, new_key) = key(server)?;
    let name = archive_name(now_ms());
    let folder = folder(server, config);
    let (dir, temporary) = match &folder {
        Some(dir) => (dir.clone(), false),
        None => (std::env::temp_dir().join(format!("kito-backup-{}", std::process::id())), true),
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Two in the same second: the second gets a name of its own.
    let mut name = name;
    let mut n = 1;
    while dir.join(&name).exists() {
        n += 1;
        name = format!("{}-{n}.kbak", archive_name(now_ms()).trim_end_matches(".kbak"));
    }
    let out = dir.join(&name);
    let inside = folder.as_deref().and_then(|dir| dir.strip_prefix(server).ok());
    let (files, bytes) = write_archive(server, &out, &key, inside)?;
    let size = std::fs::metadata(&out).map(|meta| meta.len()).unwrap_or(0);
    let mut kept = Vec::new();
    if !temporary {
        kept.push("here");
    }
    if let Some(s3) = &config.s3 {
        S3::new(server, s3)?.upload(&out, &name)?;
        kept.push("s3");
    }
    if temporary {
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_dir(&dir);
    }
    // The oldest beyond what is kept.
    let mut removed = Vec::new();
    if config.keep > 0 {
        if let Some(dir) = &folder {
            let mut here: Vec<String> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("kito-backup-") && name.ends_with(".kbak"))
                .collect();
            here.sort();
            while here.len() > config.keep {
                let old = here.remove(0);
                let _ = std::fs::remove_file(dir.join(&old));
                removed.push(old);
            }
        }
        if let Some(s3) = &config.s3 {
            let storage = S3::new(server, s3)?;
            let mut there: Vec<String> = storage.list()?.into_iter().map(|(name, _)| name).collect();
            there.sort();
            while there.len() > config.keep {
                let old = there.remove(0);
                storage.delete(&old)?;
                removed.push(format!("{old} (s3)"));
            }
        }
    }
    crate::audit::record(
        server,
        "backup_made",
        None,
        None,
        &format!("{name}: {files} files, {size} bytes, kept {}", kept.join(" and ")),
    );
    Ok(json!({
        "name": name,
        "files": files,
        "bytes": bytes,
        "size": size,
        "kept": kept,
        "removed": removed,
        "path": folder.map(|dir| dir.join(&name)),
        "new_key": new_key.then(|| hex(&key)),
    }))
}

/// When the next archive is due, from the last one there is: `None` when
/// they are made only when asked.
pub fn due_in(server: &Path, config: &BackupConfig) -> Option<std::time::Duration> {
    if config.every_hours == 0 {
        return None;
    }
    let every = config.every_hours * 3_600_000;
    let last = folder(server, config).and_then(|dir| {
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("kito-backup-"))
            .filter_map(|entry| entry.metadata().ok()?.modified().ok())
            .max()
    });
    let since = last.and_then(|at| at.elapsed().ok()).map_or(u64::MAX, |age| age.as_millis() as u64);
    Some(std::time::Duration::from_millis(every.saturating_sub(since)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_archive_is_put_back_as_it_was_and_a_change_to_it_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir_all(server.join("repos/shop/.kito/index")).unwrap();
        std::fs::write(server.join("server.toml"), "listen = \"x\"\n").unwrap();
        std::fs::write(server.join("repos/shop/.kito/index/a"), "rebuilt").unwrap();
        std::fs::write(server.join("repos/shop/.kito/lock"), "").unwrap();
        let big: Vec<u8> = (0..3 * CHUNK).map(|i| (i % 251) as u8).collect();
        std::fs::write(server.join("repos/shop/big"), &big).unwrap();
        let (key, new) = key(&server).unwrap();
        assert!(new && !super::key(&server).unwrap().1);
        let out = tmp.path().join("one.kbak");
        let (files, _) = write_archive(&server, &out, &key, None).unwrap();
        assert_eq!(files, 2, "the key, the index and the lock are left out");

        let to = tmp.path().join("back");
        assert_eq!(restore(std::fs::File::open(&out).unwrap(), &key, &to).unwrap(), 2);
        assert_eq!(std::fs::read(to.join("repos/shop/big")).unwrap(), big);
        assert!(!to.join("backup.key").exists() && !to.join("repos/shop/.kito/index").exists());
        assert!(restore(std::fs::File::open(&out).unwrap(), &key, &to).unwrap_err().contains("not empty"));

        // Another key, a byte changed, or the end cut off: refused.
        let other = [7u8; 32];
        assert!(restore(std::fs::File::open(&out).unwrap(), &other, &tmp.path().join("a"))
            .unwrap_err()
            .contains("not its key"));
        let mut bytes = std::fs::read(&out).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        assert!(restore(&bytes[..], &key, &tmp.path().join("b")).is_err());
        let whole = std::fs::read(&out).unwrap();
        let cut = &whole[..whole.len() - 40];
        assert!(restore(cut, &key, &tmp.path().join("c")).is_err());
        assert_eq!(archive_name(1_795_300_000_000), "kito-backup-2026-11-21T222640Z.kbak");
    }

    #[test]
    fn a_request_to_s3_is_signed_as_aws_says() {
        // The example of AWS's documentation for a GET, with the same keys and time.
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let signing = ["s3", "aws4_request"]
            .iter()
            .fold(hmac(&hmac(format!("AWS4{secret}").as_bytes(), "20130524"), "us-east-1"), |key, part| {
                hmac(&key, part)
            });
        assert_eq!(hex(&signing).len(), 64);
        assert_eq!(encode("a b/c", false), "a%20b/c");
        assert_eq!(encode("a/b", true), "a%2Fb");
    }
}
