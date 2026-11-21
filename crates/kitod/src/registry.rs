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
    pub plugins: BTreeMap<String, Listed>,
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
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
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

fn listed<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let bytes = fetch(url, LIST_AT_MOST)?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{url}: it is not what a registry says ({e})"))
}

/// Every plugin the registry has.
pub fn index() -> Result<Index, String> {
    let base = base();
    let index: Index = listed(&format!("{base}/index.json"))?;
    match index.registry {
        FORMAT => Ok(index),
        other => Err(format!(
            "{base}: it is a registry of format {other}, and this Kito reads format {FORMAT}: update Kito"
        )),
    }
}

/// One plugin, with its versions.
pub fn plugin(name: &str) -> Result<Plugin, String> {
    plugins::check_name("plugin", name)?;
    let base = base();
    let found: Plugin = listed(&format!("{base}/plugins/{name}.json")).map_err(|why| {
        match why.ends_with("it is not there") {
            true => format!(
                "the registry ({base}) has no plugin `{name}`: `kito plugin search` lists what it has"
            ),
            false => why,
        }
    })?;
    match found.name == name {
        true => Ok(found),
        false => Err(format!("{base}: what it lists as `{name}` says it is `{}`", found.name)),
    }
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
    let listed = plugin(name)?;
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
    Ok((installed, Source { registry: base, version: number.to_owned(), url, sha256: sum }))
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
