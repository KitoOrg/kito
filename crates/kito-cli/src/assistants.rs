//! The coding assistants Kito knows by name, which of them this machine
//! has, and which ones a new project gets (ADR 0011).
//!
//! An assistant is set up by `kito assistant add <name>`, the same for
//! every one: Claude Code by Kito itself, the others by the plugin that
//! knows them, installed and enabled on the way when needed.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// An assistant Kito can find on a machine.
#[derive(Debug, PartialEq, Eq)]
pub struct Known {
    /// What `kito assistant add` is asked for.
    pub name: &'static str,
    /// As people call it.
    pub title: &'static str,
    /// Its program, looked for on the `PATH`.
    pub program: &'static str,
    /// Its settings, in the home directory.
    pub dir: &'static str,
    /// The plugin that sets it up; `None`: Kito itself.
    pub plugin: Option<&'static str>,
}

pub const KNOWN: [Known; 3] = [
    Known { name: "claude", title: "Claude Code", program: "claude", dir: ".claude", plugin: None },
    Known { name: "codex", title: "Codex", program: "codex", dir: ".codex", plugin: Some("codex") },
    Known { name: "zcode", title: "ZCode", program: "zcode", dir: ".zcode", plugin: Some("zcode") },
];

/// The assistant of that name, if Kito knows it.
pub fn known(name: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|one| one.name == name)
}

/// How an assistant is said: its title when Kito knows it.
pub fn title(name: &str) -> String {
    known(name).map_or_else(|| name.to_owned(), |one| one.title.to_owned())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Whether `program` is on the `PATH`.
fn on_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else { return false };
    let names: Vec<String> = match cfg!(windows) {
        true => [".exe", ".cmd", ".bat", ""].iter().map(|suffix| format!("{program}{suffix}")).collect(),
        false => vec![program.to_owned()],
    };
    std::env::split_paths(&paths).any(|dir| names.iter().any(|name| dir.join(name).is_file()))
}

/// Where this assistant was found on this machine: its program, or its
/// settings; `None`: not found.
pub fn found(one: &Known) -> Option<String> {
    if on_path(one.program) {
        return Some(format!("`{}` on the PATH", one.program));
    }
    let dir = home()?.join(one.dir);
    dir.is_dir().then(|| format!("{}", Path::new("~").join(one.dir).display()))
}

/// The assistants this machine has.
pub fn on_this_machine() -> Vec<&'static Known> {
    KNOWN.iter().filter(|one| found(one).is_some()).collect()
}

/// The personal configuration, as a document that keeps what it holds.
fn personal() -> Result<(PathBuf, toml_edit::DocumentMut), String> {
    let path = kitod::identity::personal_config_path()
        .ok_or("no place for a personal configuration: set HOME or KITO_CONFIG_DIR")?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, doc))
}

/// The assistants a new project gets, as the person said; `None`: they
/// never said.
pub fn defaults() -> Option<Vec<String>> {
    let (_, doc) = personal().ok()?;
    let list = doc.get("assistants")?.get("default")?.as_array()?;
    Some(list.iter().filter_map(|value| value.as_str().map(str::to_owned)).collect())
}

/// Says which assistants a new project gets. Answers where it was written.
pub fn set_defaults(names: &[String]) -> Result<PathBuf, String> {
    let (path, mut doc) = personal()?;
    let mut list = toml_edit::Array::new();
    for name in names {
        list.push(name.as_str());
    }
    if !doc.contains_table("assistants") {
        doc["assistants"] = toml_edit::table();
    }
    doc["assistants"]["default"] = toml_edit::value(list);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, doc.to_string()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Whether somebody is at the terminal to be asked.
pub fn can_ask() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Asks a question that is answered yes by default.
pub fn ask_yes(question: &str) -> bool {
    print!("{question} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    !matches!(answer.trim().to_lowercase().as_str(), "n" | "no")
}

/// Which assistants a new project gets: what was said for it, what the
/// person said once, what they answer now, or, with nobody to ask, Claude
/// Code as Kito always did. `others`: the assistants found and not set up,
/// to be named.
/// `may_ask`: false when the answer is read by a program (`--json`).
pub fn for_new_project(
    chosen: &[String],
    none: bool,
    no_claude: bool,
    may_ask: bool,
) -> (Vec<String>, Vec<&'static Known>) {
    let without_claude = |names: Vec<String>| -> Vec<String> {
        names.into_iter().filter(|name| !(no_claude && name == "claude")).collect()
    };
    if none {
        return (Vec::new(), Vec::new());
    }
    if !chosen.is_empty() {
        return (without_claude(chosen.to_vec()), Vec::new());
    }
    // Who asked for projects to be left alone is not asked.
    if crate::claude::disabled() {
        return (Vec::new(), Vec::new());
    }
    if let Some(said) = defaults() {
        return (without_claude(said), Vec::new());
    }
    let here = on_this_machine();
    let askable: Vec<&Known> =
        here.iter().copied().filter(|one| !(no_claude && one.name == "claude")).collect();
    if may_ask && can_ask() && !askable.is_empty() {
        let mut wanted = Vec::new();
        for one in askable {
            if ask_yes(&format!("{} is on this machine. Set it up for Kito in this project?", one.title)) {
                wanted.push(one.name.to_owned());
            }
        }
        println!("(`kito assistant default <name>…` says it once, for every new project.)");
        return (wanted, Vec::new());
    }
    let wanted = without_claude(vec!["claude".to_owned()]);
    // Who said no to Claude Code is not offered it again.
    let others = here
        .into_iter()
        .filter(|one| !wanted.iter().any(|name| name == one.name) && !(no_claude && one.name == "claude"))
        .collect();
    (wanted, others)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_assistants_are_found_by_name() {
        assert_eq!(known("codex").map(|one| one.plugin), Some(Some("codex")));
        assert_eq!(known("claude").map(|one| one.plugin), Some(None));
        assert_eq!(title("claude"), "Claude Code");
        assert_eq!(title("cursor"), "cursor");
    }
}
