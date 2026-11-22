//! The server's audit log (ADR 0017): who did what to its people, rules
//! and repositories, every sign-in, and every request.
//!
//! `audit/log.jsonl` keeps what changes the server, forever;
//! `audit/requests-<day>.jsonl` keeps each request, for 90 days. Each line
//! carries the hash of the line before it in its file (`prev`) and its own
//! (`hash`): a line taken out or changed breaks the chain, which
//! `kito-server audit verify` finds.

use ring::digest::{digest, SHA256};
use serde_json::{json, Value};
use std::io::Read as _;
use std::path::{Path, PathBuf};

const DIR: &str = "audit";
const LOG: &str = "log.jsonl";
/// How long the log of requests is kept, in days.
pub const REQUESTS_KEPT_DAYS: u64 = 90;
const DAY_MS: u64 = 86_400_000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// `2026-11-22`, the day of a time in milliseconds since 1970 (UTC).
pub fn day_of(ms: u64) -> String {
    let days = (ms / DAY_MS) as i64;
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn dir_of(server: &Path) -> PathBuf {
    server.join(DIR)
}

/// One writer at a time, across the processes of the server: a lock file
/// that says whose it is. Its holder removes it, and nobody else. One
/// older than a few seconds is taken over (its writer died, and a crash
/// must not block the server for ever); a holder it was taken from finds
/// out before it writes (`held`), and leaves the file to whoever has it.
pub(crate) struct Lock {
    path: PathBuf,
    /// What this holder wrote in the file: no other holder's says the same.
    mine: String,
}

/// How long a lock is waited for before it is given up, how long between
/// two tries, and how old one is when it is taken over. The tries are
/// close together: whoever writes over and over lets go for a moment
/// each time, and whoever waits is to find that moment.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const LOCK_PAUSE: std::time::Duration = std::time::Duration::from_millis(1);
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(5);

impl Lock {
    /// `None`: somebody else holds it, and went on holding it.
    pub(crate) fn take(dir: &Path) -> Option<Lock> {
        use std::io::Write as _;
        let path = dir.join(".lock");
        let mine = holder();
        let asked = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    if file.write_all(mine.as_bytes()).is_ok() {
                        return Some(Lock { path, mine });
                    }
                    // A lock that does not say whose it is, is nobody's.
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
                Err(_) if asked.elapsed() >= LOCK_WAIT => return None,
                Err(_) => {
                    // Taken over when it is old, if it is still the one
                    // that was looked at: another that waited may have
                    // taken it over just now.
                    let taken_over = stale(&path).is_some_and(|theirs| {
                        std::fs::read_to_string(&path).is_ok_and(|now| now == theirs)
                            && std::fs::remove_file(&path).is_ok()
                    });
                    if !taken_over {
                        std::thread::sleep(LOCK_PAUSE);
                    }
                }
            }
        }
    }

    /// `true` while the lock is this holder's. One held for longer than a
    /// writer is given was taken over: whoever held it writes nothing more.
    pub(crate) fn held(&self) -> bool {
        std::fs::read_to_string(&self.path).is_ok_and(|said| said == self.mine)
    }
}

/// What a lock says when it is older than a writer is given: whose it was.
fn stale(path: &Path) -> Option<String> {
    let said = std::fs::read_to_string(path).ok()?;
    let age = std::fs::metadata(path).and_then(|meta| meta.modified()).ok()?.elapsed().ok()?;
    (age > LOCK_STALE).then_some(said)
}

/// What a holder writes in its lock: a value no other holder has.
fn holder() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TAKEN: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    match getrandom::getrandom(&mut bytes) {
        Ok(()) => hex(&bytes),
        // No randomness: this process, this moment and how many it took.
        Err(_) => format!("{}-{}-{}", std::process::id(), now_ms(), TAKEN.fetch_add(1, Ordering::Relaxed)),
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Another's, once it was taken over: theirs to remove.
        if self.held() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash of a line: of the hash before it and of what it says.
fn hash_of(prev: &str, entry: &Value) -> String {
    let said = format!("{prev}\n{entry}");
    hex(digest(&SHA256, said.as_bytes()).as_ref())
}

/// The hash of the last line of a file: read from its end, whatever its
/// size, so that a request is not slower when the day is long.
fn last_hash(path: &Path) -> String {
    use std::io::{Seek, SeekFrom};
    let tail = (|| -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let length = file.metadata()?.len();
        let mut back = 4096u64;
        loop {
            let from = length.saturating_sub(back);
            file.seek(SeekFrom::Start(from))?;
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            // A whole last line: after a line ending, or from the start of the file.
            let trimmed = text.trim_end();
            if let Some(at) = trimmed.rfind('\n') {
                return Ok(trimmed[at + 1..].to_owned());
            }
            if from == 0 {
                return Ok(trimmed.to_owned());
            }
            back *= 4;
        }
    })();
    tail.ok()
        .and_then(|line| serde_json::from_str::<Value>(&line).ok())
        .and_then(|entry| entry["hash"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "start".to_owned())
}

/// Appends one entry to a file of the log, chained to the line before.
fn append(server: &Path, file: &str, mut entry: Value) {
    use std::io::Write as _;
    let dir = dir_of(server);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Some(_lock) = Lock::take(&dir) else {
        // Said where whoever runs the server reads: a line the log does
        // not have is not passed over in silence.
        eprintln!(
            "kito-server: the audit log is held by something that does not finish: not written: {entry}"
        );
        return;
    };
    let path = dir.join(file);
    let prev = last_hash(&path);
    entry["prev"] = json!(prev);
    let hash = hash_of(&prev, &entry);
    entry["hash"] = json!(hash);
    if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(out, "{entry}");
    }
}

/// Keeps something that changed the server: people, access, rules,
/// repositories, plugins, sign-ins, backups. `by`: who, or none for
/// whoever ran `kito-server` on its machine.
pub fn record(server: &Path, kind: &str, by: Option<&str>, repo: Option<&str>, what: &str) {
    let entry = json!({
        "at_ms": now_ms(),
        "kind": kind,
        "by": by.unwrap_or("the server's machine"),
        "repo": repo,
        "what": what,
    });
    append(server, LOG, entry);
}

/// Keeps one request: who asked what of which repository, and whether it
/// was answered. The files of days older than 90 are taken out.
pub fn request(server: &Path, by: &str, repo: Option<&str>, method: &str, answered: bool) {
    let now = now_ms();
    let file = format!("requests-{}.jsonl", day_of(now));
    let dir = dir_of(server);
    if !dir.join(&file).exists() {
        prune(&dir, now);
    }
    let entry =
        json!({ "at_ms": now, "kind": "request", "by": by, "repo": repo, "what": method, "ok": answered });
    append(server, &file, entry);
}

fn prune(dir: &Path, now: u64) {
    let oldest = day_of(now.saturating_sub(REQUESTS_KEPT_DAYS * DAY_MS));
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(day) = name.strip_prefix("requests-").and_then(|rest| rest.strip_suffix(".jsonl")) {
            if day < oldest.as_str() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// What to read of the log.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub requests: bool,
    pub who: Option<String>,
    pub kind: Option<String>,
    pub repo: Option<String>,
    pub since_ms: Option<u64>,
    pub limit: usize,
}

/// The entries that fit, the latest first.
pub fn read(server: &Path, filter: &Filter) -> Vec<Value> {
    let dir = dir_of(server);
    let mut files: Vec<PathBuf> = match filter.requests {
        false => vec![dir.join(LOG)],
        true => {
            let mut days: Vec<PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("requests-"))
                })
                .collect();
            days.sort();
            days
        }
    };
    files.reverse();
    let lower = |text: &str| text.to_lowercase();
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for line in text.lines().rev() {
            let Ok(mut entry) = serde_json::from_str::<Value>(line) else { continue };
            let at = entry["at_ms"].as_u64().unwrap_or(0);
            if filter.since_ms.is_some_and(|since| at < since) {
                continue;
            }
            let has = |key: &str, wanted: &Option<String>| {
                wanted
                    .as_ref()
                    .is_none_or(|wanted| lower(entry[key].as_str().unwrap_or("")).contains(&lower(wanted)))
            };
            if !has("by", &filter.who) || !has("kind", &filter.kind) || !has("repo", &filter.repo) {
                continue;
            }
            if let Some(map) = entry.as_object_mut() {
                map.remove("prev");
                map.remove("hash");
            }
            found.push(entry);
            if found.len() >= filter.limit.max(1) {
                return found;
            }
        }
    }
    found
}

/// Walks every chain of the log. `Ok`: how many lines, all in order;
/// `Err`: the first that is not, and why.
pub fn verify(server: &Path) -> Result<usize, String> {
    let dir = dir_of(server);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    let mut lines = 0;
    for file in files {
        let name = file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let mut prev = "start".to_owned();
        for (n, line) in std::fs::read_to_string(&file).unwrap_or_default().lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let mut entry: Value = serde_json::from_str(line)
                .map_err(|_| format!("{name}, line {}: not an entry of the log", n + 1))?;
            let hash = entry["hash"].as_str().unwrap_or_default().to_owned();
            if entry["prev"].as_str() != Some(prev.as_str()) {
                return Err(format!("{name}, line {}: the line before it was taken out or changed", n + 1));
            }
            if let Some(map) = entry.as_object_mut() {
                map.remove("hash");
            }
            if hash_of(&prev, &entry) != hash {
                return Err(format!("{name}, line {}: it was changed after it was written", n + 1));
            }
            prev = hash;
            lines += 1;
        }
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_is_a_chain_and_a_change_breaks_it() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "user_added", None, None, "carol (member)");
        record(dir.path(), "role_changed", Some("ada"), Some("shop"), "carol may write in shop");
        request(dir.path(), "carol", Some("shop"), "status", true);
        assert_eq!(verify(dir.path()), Ok(3));
        let seen = read(dir.path(), &Filter { limit: 10, ..Filter::default() });
        assert_eq!((seen.len(), seen[0]["by"].as_str()), (2, Some("ada")));
        assert!(seen[0].get("hash").is_none());
        let asked = read(
            dir.path(),
            &Filter { requests: true, who: Some("CAROL".into()), limit: 10, ..Filter::default() },
        );
        assert_eq!(asked[0]["what"], "status");
        // Somebody edits a line by hand.
        let log = dir.path().join("audit/log.jsonl");
        let text = std::fs::read_to_string(&log).unwrap().replace("may write", "may maintain");
        std::fs::write(&log, text).unwrap();
        assert!(verify(dir.path()).unwrap_err().contains("log.jsonl, line 2: it was changed"));
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(1_795_300_000_000), "2026-11-21");
    }
}
