//! The registry of plugins (ADR 0008): files that say what a name
//! installs, read over HTTPS, and a plugin installed from what they list.
//!
//! ```text
//! <registry>/index.json            every plugin, one line each
//! <registry>/plugins/<name>.json   one plugin: its versions and their files
//! ```
//!
//! What is downloaded is checked against the SHA-256 the registry gave,
//! unpacked apart, and installed only if the manifest inside is the
//! plugin and the version that were asked for. Nothing of it is run.
//!
//! A registry whose key is known is signed (ADR 0009): each of its files
//! has a signature beside it (`index.json.sig`), made with that key, and
//! a file without one, or with another's, is not taken. Its index is
//! numbered, and does not go back.

use crate::plugins::{self, Installed, Manifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// The registry asked when no other is said.
pub const DEFAULT: &str = "https://kito.sh/registry";
/// In the environment: another registry (a team's own; `file://` too).
pub const ENV_REGISTRY: &str = "KITO_REGISTRY";
/// In the environment: the keys the registry's files are signed with,
/// as hexadecimal, with commas between them. With one, a file that is not
/// signed by it is not taken.
pub const ENV_REGISTRY_KEY: &str = "KITO_REGISTRY_KEY";
/// The keys of the registry at kito.sh. None yet: nothing is published
/// there, and until its key is made and written here its files are taken
/// as its address gives them.
const PINNED: &[&str] = &[];
/// The version of the registry's format this reads.
pub const FORMAT: u32 = 1;
/// The most a file of the registry, and an archive, may weigh.
const LIST_AT_MOST: u64 = 4 * 1024 * 1024;
const ARCHIVE_AT_MOST: u64 = 256 * 1024 * 1024;
/// The most an archive may unpack to, and the most files it may hold.
const UNPACKED_AT_MOST: u64 = 1024 * 1024 * 1024;
const FILES_AT_MOST: usize = 4096;

/// `index.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Index {
    pub registry: u32,
    /// How many times the registry was signed: a signed index with a
    /// lower number than one already seen is an old one, served again.
    #[serde(skip_serializing_if = "is_zero")]
    pub serial: u64,
    pub plugins: BTreeMap<String, Listed>,
}

fn is_zero(serial: &u64) -> bool {
    *serial == 0
}

/// A plugin as the index lists it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Listed {
    pub about: String,
    pub latest: String,
}

/// `plugins/<name>.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Plugin {
    pub name: String,
    pub about: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub homepage: String,
    pub versions: BTreeMap<String, Version>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Version {
    /// The version of the daemon's API it was written for.
    pub api: u32,
    /// Its archive for each platform (`linux-x86_64`).
    pub files: BTreeMap<String, File>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct File {
    /// Where the archive is: anywhere that speaks HTTPS, or beside the
    /// registry's own files when it is no address of its own.
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// Where a plugin that was installed came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Source {
    pub registry: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    /// The registry's files were checked against its key.
    pub signed: bool,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok()).collect()
}

/// The keys a registry's files must be signed with: the ones said in the
/// environment, or for the registry at kito.sh the ones Kito was built
/// with. None: the registry is taken as its address gives it.
pub fn keys_for(base: &str) -> Result<Vec<Vec<u8>>, String> {
    let said = std::env::var(ENV_REGISTRY_KEY).ok().filter(|said| !said.trim().is_empty());
    let keys: Vec<&str> = match &said {
        Some(said) => said.split(',').map(str::trim).collect(),
        None if base == DEFAULT => PINNED.to_vec(),
        None => Vec::new(),
    };
    keys.into_iter()
        .map(|key| {
            // A key that cannot be read is never "no key": the registry
            // would be taken unchecked by whoever meant it to be checked.
            unhex(key).filter(|bytes| bytes.len() == 32).ok_or_else(|| {
                format!("{ENV_REGISTRY_KEY}: `{key}` is not a key of a registry (64 hexadecimal digits)")
            })
        })
        .collect()
}

/// `true` if this person's registry is one whose files are checked
/// against a key.
pub fn checked() -> Result<bool, String> {
    Ok(!keys_for(&base())?.is_empty())
}

/// The registry this person's Kito asks.
pub fn base() -> String {
    let said =
        std::env::var(ENV_REGISTRY).ok().map(|said| said.trim().to_owned()).filter(|said| !said.is_empty());
    said.unwrap_or_else(|| DEFAULT.to_owned()).trim_end_matches('/').to_owned()
}

/// This machine, as releases name platforms.
pub fn platform() -> String {
    let os = std::env::consts::OS;
    let arch = match (os, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    format!("{os}-{arch}")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Reads what is at an address: HTTPS, a file (`file://`), or HTTP on
/// this machine. Nothing else, and never more than `at_most` bytes.
fn fetch(url: &str, at_most: u64) -> Result<Vec<u8>, String> {
    let too_big =
        || format!("{url}: it is more than {} MB, which nothing of a registry is", at_most / (1024 * 1024));
    if let Some(path) = url.strip_prefix("file://") {
        let bytes = std::fs::read(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{url}: it is not there"),
            _ => format!("{url}: {e}"),
        })?;
        return if bytes.len() as u64 > at_most { Err(too_big()) } else { Ok(bytes) };
    }
    let local = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|at| url.strip_prefix(at).is_some_and(|rest| rest.starts_with([':', '/'])));
    if !url.starts_with("https://") && !local {
        return Err(format!(
            "{url}: a registry and what it lists are read over https, or from a file (file://)"
        ));
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(600))
        .user_agent(concat!("kito/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = match agent.get(url).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Err(format!("{url}: it is not there")),
        Err(ureq::Error::Status(code, _)) => return Err(format!("{url}: the server answered {code}")),
        Err(e) => return Err(format!("{url}: {e}")),
    };
    let mut bytes = Vec::new();
    response.into_reader().take(at_most + 1).read_to_end(&mut bytes).map_err(|e| format!("{url}: {e}"))?;
    if bytes.len() as u64 > at_most {
        return Err(too_big());
    }
    Ok(bytes)
}

/// `true` if the bytes of a file of the registry are signed by one of
/// its keys (`<file>.sig` beside it); `false` if no key is known for the
/// registry, which is then taken as it is. `Err`: a key is known, and
/// the file is not signed by it.
fn signed(url: &str, bytes: &[u8], keys: &[Vec<u8>]) -> Result<bool, String> {
    if keys.is_empty() {
        return Ok(false);
    }
    let refuse = |why: &str| format!("{url}: {why}: nothing of it is taken");
    let signature =
        fetch(&format!("{url}.sig"), 1024).map_err(|why| match why.ends_with("it is not there") {
            true => refuse("it is not signed, and this registry's files are checked against its key"),
            false => why,
        })?;
    let signature = unhex(&String::from_utf8_lossy(&signature))
        .filter(|signature| signature.len() == 64)
        .ok_or_else(|| refuse("what is beside it is not a signature"))?;
    let by = |key: &Vec<u8>| {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
            .verify(bytes, &signature)
            .is_ok()
    };
    match keys.iter().any(by) {
        true => Ok(true),
        false => Err(refuse("it is not signed with a key of this registry")),
    }
}

/// A file of the registry, and whether it was checked against a key.
fn listed<T: serde::de::DeserializeOwned>(url: &str, keys: &[Vec<u8>]) -> Result<(T, bool), String> {
    let bytes = fetch(url, LIST_AT_MOST)?;
    let signed = signed(url, &bytes, keys)?;
    let said =
        serde_json::from_slice(&bytes).map_err(|e| format!("{url}: it is not what a registry says ({e})"))?;
    Ok((said, signed))
}

/// Where this person's Kito remembers how far each signed registry had
/// got: `registries.json`, next to their plugins.
fn seen_file() -> Option<PathBuf> {
    Some(plugins::plugins_dir()?.parent()?.join("registries.json"))
}

/// A signed index does not go back: one numbered lower than the last
/// this machine saw of that registry is an old one, and whoever serves
/// it again wants an old version installed.
fn not_older(base: &str, serial: u64, seen: Option<&Path>) -> Result<(), String> {
    let Some(seen) = seen else { return Ok(()) };
    let mut all: BTreeMap<String, u64> = std::fs::read_to_string(seen)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let before = all.get(base).copied().unwrap_or(0);
    if serial < before {
        return Err(format!(
            "{base}: its index is an older one (number {serial}) than this machine has seen (number {before}): a \
             registry does not go back, and nothing of it is taken"
        ));
    }
    if serial > before {
        all.insert(base.to_owned(), serial);
        let _ = std::fs::create_dir_all(seen.parent().unwrap_or(seen));
        let _ = std::fs::write(seen, serde_json::to_string_pretty(&all).unwrap_or_default());
    }
    Ok(())
}

fn index_of(base: &str, keys: &[Vec<u8>], seen: Option<&Path>) -> Result<(Index, bool), String> {
    let (index, signed): (Index, bool) = listed(&format!("{base}/index.json"), keys)?;
    if index.registry != FORMAT {
        return Err(format!(
            "{base}: it is a registry of format {}, and this Kito reads format {FORMAT}: update Kito",
            index.registry
        ));
    }
    if signed {
        not_older(base, index.serial, seen)?;
    }
    Ok((index, signed))
}

/// Every plugin the registry has.
pub fn index() -> Result<Index, String> {
    let base = base();
    index_of(&base, &keys_for(&base)?, seen_file().as_deref()).map(|(index, _)| index)
}

fn plugin_of(
    base: &str,
    keys: &[Vec<u8>],
    seen: Option<&Path>,
    name: &str,
) -> Result<(Plugin, bool), String> {
    plugins::check_name("plugin", name)?;
    let url = format!("{base}/plugins/{name}.json");
    let (found, signed): (Plugin, bool) =
        listed(&url, keys).map_err(|why| match why.ends_with("it is not there") {
            true => format!(
                "the registry ({base}) has no plugin `{name}`: `kito plugin search` lists what it has"
            ),
            false => why,
        })?;
    if found.name != name {
        return Err(format!("{base}: what it lists as `{name}` says it is `{}`", found.name));
    }
    if signed {
        // A plugin's file is signed by itself, so an old one is still
        // signed. The index, which does not go back, says which version
        // is the latest: a file that does not have it is an old file.
        let (index, _) = index_of(base, keys, seen)?;
        let latest = index.plugins.get(name).map(|listed| listed.latest.as_str());
        if !latest.is_some_and(|latest| found.versions.contains_key(latest)) {
            return Err(format!(
                "{url}: it does not have the version the registry's index says is the latest ({}): it is an \
                 older file, and nothing of it is taken",
                latest.unwrap_or("none: the index does not list this plugin")
            ));
        }
    }
    Ok((found, signed))
}

/// One plugin, with its versions.
pub fn plugin(name: &str) -> Result<Plugin, String> {
    let base = base();
    plugin_of(&base, &keys_for(&base)?, seen_file().as_deref(), name).map(|(plugin, _)| plugin)
}

/// A plugin this person has, and the later version the registry lists
/// that fits this Kito and this machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Outdated {
    pub name: String,
    pub installed: String,
    pub latest: String,
}

/// The installed plugins the registry has a later version of. A plugin
/// the registry does not list (installed from a directory) is not one;
/// `Err` is for a registry that could not be asked at all.
pub fn outdated() -> Result<Vec<Outdated>, String> {
    let index = index()?;
    let mut found = Vec::new();
    for installed in plugins::all_installed() {
        let name = &installed.manifest.name;
        if !index.plugins.contains_key(name) {
            continue;
        }
        let Ok(listed) = plugin(name) else { continue };
        let Ok((latest, _)) = pick(&listed, None) else { continue };
        if numbers(latest) > numbers(&installed.manifest.version) {
            found.push(Outdated {
                name: name.clone(),
                installed: installed.manifest.version.clone(),
                latest: latest.to_owned(),
            });
        }
    }
    Ok(found)
}

/// A new key for a registry: what is kept secret, to sign with, and what
/// is told to everybody, to check with. Both as hexadecimal.
pub fn new_key() -> Result<(String, String), String> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| format!("no randomness to make a key from: {e}"))?;
    let public = public_of(&hex(&seed))?;
    Ok((hex(&seed), public))
}

fn pair_of(secret: &str) -> Result<ring::signature::Ed25519KeyPair, String> {
    let seed = unhex(secret).filter(|seed| seed.len() == 32).ok_or("it is not the key of a registry")?;
    ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|_| "it is not the key of a registry".to_owned())
}

/// What is told to everybody of a key that is kept secret.
pub fn public_of(secret: &str) -> Result<String, String> {
    use ring::signature::KeyPair as _;
    Ok(hex(pair_of(secret)?.public_key().as_ref()))
}

/// Signs a registry kept in a directory with its secret key: the index
/// gets the next number, and every file a signature beside it. A registry
/// whose files do not agree is not signed: nobody could install from it.
/// Returns the index's number and how many plugins were signed.
pub fn sign_dir(dir: &Path, secret: &str) -> Result<(u64, usize), String> {
    let pair = pair_of(secret)?;
    let read = |path: &Path| std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()));
    let write = |path: &Path, bytes: &[u8]| {
        std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
    };
    let index_path = dir.join("index.json");
    let mut said: serde_json::Value = serde_json::from_slice(&read(&index_path)?)
        .map_err(|e| format!("{}: it is not what a registry says ({e})", index_path.display()))?;
    let index: Index =
        serde_json::from_value(said.clone()).map_err(|e| format!("{}: {e}", index_path.display()))?;
    if index.registry != FORMAT {
        return Err(format!("{}: it is not a registry of format {FORMAT}", index_path.display()));
    }
    // Every plugin the index lists has its file, and the file the version
    // the index says is the latest.
    let mut files = Vec::new();
    for (name, listed) in &index.plugins {
        let path = dir.join("plugins").join(format!("{name}.json"));
        let bytes = read(&path)?;
        let plugin: Plugin =
            serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        if plugin.name != *name || !plugin.versions.contains_key(&listed.latest) {
            return Err(format!(
                "{}: the index says `{name}` is at {}, and this file is of `{}` and does not have that version: \
                 nothing was signed",
                path.display(),
                listed.latest,
                plugin.name
            ));
        }
        files.push((path, bytes));
    }
    let serial = index.serial + 1;
    said["serial"] = serial.into();
    let mut bytes = serde_json::to_vec_pretty(&said).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    write(&index_path, &bytes)?;
    files.push((index_path, bytes));
    for (path, bytes) in &files {
        let mut beside = path.clone().into_os_string();
        beside.push(".sig");
        write(Path::new(&beside), format!("{}\n", hex(pair.sign(bytes).as_ref())).as_bytes())?;
    }
    Ok((serial, files.len() - 1))
}

/// A version as numbers, for telling which is later: `0.10.1` after `0.9`.
fn numbers(version: &str) -> Vec<u64> {
    version.split(['.', '-', '+']).map(|part| part.parse().unwrap_or(0)).collect()
}

/// The version to install: the one asked for, or the latest that fits
/// this Kito's API and this machine.
pub fn pick<'a>(plugin: &'a Plugin, wanted: Option<&str>) -> Result<(&'a str, &'a File), String> {
    let (api, platform) = (crate::rpc::API, platform());
    let fits = |version: &Version| version.api <= api && version.files.contains_key(&platform);
    if let Some(wanted) = wanted {
        let Some((number, version)) = plugin.versions.get_key_value(wanted) else {
            let known: Vec<&str> = plugin.versions.keys().map(String::as_str).collect();
            return Err(format!("`{}` has no version {wanted}: it has {}", plugin.name, known.join(", ")));
        };
        if version.api > api {
            return Err(format!(
                "`{}` {wanted} was written for API {}, and this Kito speaks API {api}: update Kito",
                plugin.name, version.api
            ));
        }
        return match version.files.get(&platform) {
            Some(file) => Ok((number, file)),
            None => Err(format!("`{}` {wanted} has no build for {platform}", plugin.name)),
        };
    }
    let mut fitting: Vec<(&String, &Version)> =
        plugin.versions.iter().filter(|(_, version)| fits(version)).collect();
    fitting.sort_by_key(|(number, _)| numbers(number));
    match fitting.last() {
        Some((number, version)) => Ok((number.as_str(), &version.files[&platform])),
        None if plugin.versions.values().any(|version| version.files.contains_key(&platform)) => Err(format!(
            "every version of `{}` for {platform} was written for a later Kito than this one (API {api}): update Kito",
            plugin.name
        )),
        None => Err(format!("`{}` has no build for {platform}", plugin.name)),
    }
}

/// Unpacks a `.tar.gz` into `into`, which is empty. Only plain files and
/// directories, only inside `into`: anything else refuses the archive.
pub fn unpack(archive: &[u8], into: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let (mut files, mut total) = (0usize, 0u64);
    let entries = archive.entries().map_err(|e| format!("it is not an archive Kito can open: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("it is not an archive Kito can open: {e}"))?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let inside = path.components().all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
        if !inside || path.as_os_str().is_empty() {
            return Err(format!("it holds `{}`, which is not a place inside it", path.display()));
        }
        let to = into.join(&path);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
            continue;
        }
        if !kind.is_file() {
            return Err(format!(
                "it holds `{}`, which is not a plain file (a link, a device)",
                path.display()
            ));
        }
        files += 1;
        total += entry.size();
        if files > FILES_AT_MOST || total > UNPACKED_AT_MOST {
            return Err("it unpacks to more than a plugin is".to_owned());
        }
        std::fs::create_dir_all(to.parent().unwrap_or(into)).map_err(|e| format!("{}: {e}", to.display()))?;
        let mut out = std::fs::File::create(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        std::io::copy(&mut entry.by_ref().take(UNPACKED_AT_MOST), &mut out)
            .map_err(|e| format!("{}: {e}", to.display()))?;
        // A program stays a program.
        #[cfg(unix)]
        if entry.header().mode().is_ok_and(|mode| mode & 0o111 != 0) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755));
        }
    }
    Ok(())
}

/// The directory of an unpacked archive that is the plugin: the one that
/// holds its manifest, the archive itself or the one directory in it.
fn plugin_dir(unpacked: &Path) -> Option<PathBuf> {
    if unpacked.join(plugins::MANIFEST).is_file() {
        return Some(unpacked.to_owned());
    }
    let mut inside = std::fs::read_dir(unpacked).ok()?.flatten().map(|entry| entry.path());
    let (one, none) = (inside.next()?, inside.next());
    (none.is_none() && one.join(plugins::MANIFEST).is_file()).then_some(one)
}

/// A place of its own to unpack in, removed when done.
struct Apart(PathBuf);

impl Apart {
    fn new() -> Result<Apart, String> {
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).map_err(|e| e.to_string())?;
        let name: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("kito-plugin-{name}"));
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Apart(dir))
    }
}

impl Drop for Apart {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Installs a plugin by its name from the registry: the version asked
/// for, or the latest that fits. Anything that does not match what the
/// registry said is refused, and nothing is installed.
pub fn install(name: &str, version: Option<&str>) -> Result<(Installed, Source), String> {
    let base = base();
    let keys = keys_for(&base)?;
    let (listed, signed) = plugin_of(&base, &keys, seen_file().as_deref(), name)?;
    let (number, file) = pick(&listed, version)?;
    let url = match file.url.contains("://") {
        true => file.url.clone(),
        false => format!("{base}/{}", file.url.trim_start_matches('/')),
    };
    let archive = fetch(&url, ARCHIVE_AT_MOST)?;
    let sum = sha256_hex(&archive);
    if !sum.eq_ignore_ascii_case(file.sha256.trim()) || sum.len() != 64 {
        return Err(format!(
            "{url}: it is not the file the registry listed for `{name}` {number} (its SHA-256 is {sum}, the registry \
             says {}): nothing was installed",
            file.sha256.trim()
        ));
    }
    let apart = Apart::new()?;
    unpack(&archive, &apart.0).map_err(|why| format!("{url}: {why}: nothing was installed"))?;
    let dir = plugin_dir(&apart.0)
        .ok_or_else(|| format!("{url}: there is no {} in it: nothing was installed", plugins::MANIFEST))?;
    let manifest = Manifest::read(&dir)?;
    if manifest.name != name || manifest.version != number {
        return Err(format!(
            "{url}: the registry lists it as `{name}` {number}, and it is `{}` {}: nothing was installed",
            manifest.name, manifest.version
        ));
    }
    let installed = plugins::install_from(&dir)?;
    Ok((installed, Source { registry: base, version: number.to_owned(), url, sha256: sum, signed }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(api: u32, platforms: &[&str]) -> Version {
        let files = platforms.iter().map(|platform| ((*platform).to_owned(), File::default())).collect();
        Version { api, files }
    }

    #[test]
    fn the_latest_version_that_fits_is_the_one_installed() {
        let here = platform();
        let api = crate::rpc::API;
        let mut plugin = Plugin { name: "web".into(), ..Plugin::default() };
        plugin.versions.insert("0.9.0".into(), version(1, &[&here]));
        plugin.versions.insert("0.10.0".into(), version(api, &[&here]));
        plugin.versions.insert("0.11.0".into(), version(api + 1, &[&here]));
        plugin.versions.insert("0.12.0".into(), version(api, &["plan9-mips"]));
        // Later by its numbers, not by its letters; not one for a later Kito or another machine.
        assert_eq!(pick(&plugin, None).unwrap().0, "0.10.0");
        assert_eq!(pick(&plugin, Some("0.9.0")).unwrap().0, "0.9.0");
        assert!(pick(&plugin, Some("0.11.0")).unwrap_err().contains("update Kito"));
        assert!(pick(&plugin, Some("0.12.0")).unwrap_err().contains(&format!("no build for {here}")));
        assert!(pick(&plugin, Some("1.0.0")).unwrap_err().contains("it has 0.10.0, 0.11.0, 0.12.0, 0.9.0"));
        plugin.versions.retain(|number, _| number == "0.11.0");
        assert!(pick(&plugin, None).unwrap_err().contains("for a later Kito"));
        plugin.versions.clear();
        assert!(pick(&plugin, None).unwrap_err().contains("has no build for"));
    }

    fn archive(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let mut builder =
            tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (path, kind, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            // Written as it is, whatever it is: an archive made to do harm is.
            let name = header.as_old_mut().name.as_mut();
            name[..path.len()].copy_from_slice(path.as_bytes());
            if kind.is_symlink() {
                header.set_link_name("/etc/passwd").unwrap();
            }
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn an_archive_is_unpacked_inside_its_place_or_not_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tar::EntryType::Regular;
        let good = archive(&[
            ("kito-web/kito-web", file, b"#!/bin/sh\n"),
            ("kito-web/kito-plugin.toml", file, b"x"),
        ]);
        let into = tmp.path().join("good");
        std::fs::create_dir_all(&into).unwrap();
        unpack(&good, &into).unwrap();
        assert_eq!(std::fs::read(into.join("kito-web/kito-plugin.toml")).unwrap(), b"x");
        assert_eq!(plugin_dir(&into), Some(into.join("kito-web")));

        for (path, kind) in
            [("../out", file), ("/abs", file), ("a/../../out", file), ("link", tar::EntryType::Symlink)]
        {
            let into = tmp.path().join("bad");
            let _ = std::fs::remove_dir_all(&into);
            std::fs::create_dir_all(&into).unwrap();
            let why = unpack(&archive(&[(path, kind, b"x")]), &into).unwrap_err();
            assert!(
                why.contains("not a place inside it") || why.contains("not a plain file"),
                "{path}: {why}"
            );
        }
        assert!(!tmp.path().join("out").exists());
        assert!(unpack(b"not an archive", &tmp.path().join("bad")).is_err());
    }

    /// A registry of files with one plugin, signed with `secret`.
    fn signed_registry(dir: &Path, secret: &str, latest: &str) {
        std::fs::create_dir_all(dir.join("plugins")).unwrap();
        let index =
            serde_json::json!({ "registry": 1, "plugins": { "web": { "about": "x", "latest": latest } } });
        let serial = std::fs::read_to_string(dir.join("index.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Index>(&text).ok())
            .map_or(0, |index| index.serial);
        let mut index = index;
        index["serial"] = serial.into();
        std::fs::write(dir.join("index.json"), index.to_string()).unwrap();
        let mut versions = serde_json::Map::new();
        for number in ["0.1.0", latest] {
            versions.insert(number.to_owned(), serde_json::json!({ "api": 1, "files": {} }));
        }
        let plugin = serde_json::json!({ "name": "web", "about": "x", "versions": versions });
        std::fs::write(dir.join("plugins/web.json"), plugin.to_string()).unwrap();
        sign_dir(dir, secret).unwrap();
    }

    #[test]
    fn a_registry_whose_key_is_known_is_taken_only_as_that_key_signed_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("registry");
        let base = format!("file://{}", dir.display());
        let (secret, public) = new_key().unwrap();
        assert_eq!(public_of(&secret).unwrap(), public);
        let keys = vec![unhex(&public).unwrap()];
        let seen = tmp.path().join("registries.json");
        signed_registry(&dir, &secret, "0.1.0");

        // Checked, and said to be. With no key known, taken as it is.
        let (index, signed) = index_of(&base, &keys, Some(&seen)).unwrap();
        assert!(signed && index.serial == 1 && index.plugins.contains_key("web"));
        assert!(plugin_of(&base, &keys, Some(&seen), "web").unwrap().1);
        assert!(!index_of(&base, &[], Some(&seen)).unwrap().1);

        // Another's key, a file changed since, a file with no signature.
        let (_, other) = new_key().unwrap();
        let why = index_of(&base, &[unhex(&other).unwrap()], None).unwrap_err();
        assert!(why.contains("not signed with a key of this registry"), "{why}");
        let good = std::fs::read(dir.join("plugins/web.json")).unwrap();
        std::fs::write(
            dir.join("plugins/web.json"),
            String::from_utf8_lossy(&good).replace("\"x\"", "\"y\""),
        )
        .unwrap();
        assert!(plugin_of(&base, &keys, Some(&seen), "web").unwrap_err().contains("not signed with a key"));
        std::fs::write(dir.join("plugins/web.json"), &good).unwrap();
        let signature = std::fs::read(dir.join("index.json.sig")).unwrap();
        std::fs::remove_file(dir.join("index.json.sig")).unwrap();
        assert!(index_of(&base, &keys, Some(&seen))
            .unwrap_err()
            .contains("it is not signed, and this registry's"));
        std::fs::write(dir.join("index.json.sig"), "zz").unwrap();
        assert!(index_of(&base, &keys, Some(&seen)).unwrap_err().contains("is not a signature"));
        std::fs::write(dir.join("index.json.sig"), &signature).unwrap();
        // One of several keys is enough: a registry changes its key by having two for a while.
        assert!(index_of(&base, &[unhex(&other).unwrap(), keys[0].clone()], Some(&seen)).unwrap().1);

        // Kept from before: the first index, and the plugin's first file, signed as they were.
        let old: Vec<(&str, Vec<u8>)> =
            ["index.json", "index.json.sig", "plugins/web.json", "plugins/web.json.sig"]
                .into_iter()
                .map(|file| (file, std::fs::read(dir.join(file)).unwrap()))
                .collect();
        signed_registry(&dir, &secret, "0.2.0");
        assert_eq!(index_of(&base, &keys, Some(&seen)).unwrap().0.serial, 2);
        assert!(plugin_of(&base, &keys, Some(&seen), "web").unwrap().0.versions.contains_key("0.2.0"));
        // The plugin's old file, served again: signed, and too old for the index.
        for (file, bytes) in &old[2..] {
            std::fs::write(dir.join(file), bytes).unwrap();
        }
        let why = plugin_of(&base, &keys, Some(&seen), "web").unwrap_err();
        assert!(why.contains("the latest (0.2.0): it is an older file"), "{why}");
        // The old index too: signed, and a registry does not go back.
        for (file, bytes) in &old[..2] {
            std::fs::write(dir.join(file), bytes).unwrap();
        }
        let why = index_of(&base, &keys, Some(&seen)).unwrap_err();
        assert!(why.contains("an older one (number 1) than this machine has seen (number 2)"), "{why}");
        // A machine that never saw the later one cannot tell, and takes it.
        assert!(index_of(&base, &keys, Some(&tmp.path().join("another.json"))).is_ok());

        // A registry whose files do not agree is not signed at all.
        std::fs::write(dir.join("plugins/web.json"), r#"{"name":"other","versions":{}}"#).unwrap();
        assert!(sign_dir(&dir, &secret).unwrap_err().contains("nothing was signed"));
        assert!(sign_dir(&dir, "abc").unwrap_err().contains("not the key of a registry"));
    }

    #[test]
    fn only_https_a_file_and_this_machine_are_read() {
        for url in ["http://kito.sh/registry/index.json", "ftp://x/y", "kito.sh/index.json"] {
            assert!(fetch(url, 10).unwrap_err().contains("read over https"), "{url}");
        }
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("index.json");
        std::fs::write(&file, b"0123456789").unwrap();
        let url = format!("file://{}", file.display());
        assert_eq!(fetch(&url, 10).unwrap(), b"0123456789");
        assert!(fetch(&url, 9).unwrap_err().contains("more than"));
    }
}
