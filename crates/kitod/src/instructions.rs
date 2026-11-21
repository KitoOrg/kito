//! What agents are told in a repository, kept by Kito as metadata and
//! never in the project's files (ADR 0012).
//!
//! An instruction is a markdown file named `<folder>/<file>.md`: the
//! folder `all` is for every assistant, a folder named after one
//! (`claude`, `codex`, `zcode`, …) for that one only. They come from two
//! places, the later winning:
//!
//! 1. the kitchen attached to the repository: its `instructions/` folder;
//! 2. the repository's own, in `.kito/instructions/`. One of the same name
//!    replaces the kitchen's; an empty one takes it away.
//!
//! Kito's own rules come before both, and are not kept here: they are the
//! rules of the version that writes the files.

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The files assistants read in a project, which Kito keeps out of the
/// history and of git unless `[agents] files_in_history` says otherwise.
pub const ASSISTANT_FILES: &[&str] = &[
    "/CLAUDE.md",
    "/AGENTS.md",
    "/GEMINI.md",
    "/.mcp.json",
    "/.claude/",
    "/.codex/",
    "/.zcode/",
    "/.cursor/",
    "/.cursorrules",
    "/.windsurfrules",
    "/.github/copilot-instructions.md",
];

/// Where a repository keeps its own instructions, under `.kito/`.
pub const DIR: &str = "instructions";
/// The folder for every assistant.
pub const ALL: &str = "all";

/// One instruction as an assistant gets it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Instruction {
    /// `all/style.md`
    pub name: String,
    /// `kitchen` or `repository`.
    pub from: &'static str,
    /// It replaces the kitchen's of the same name.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub overrides: bool,
    pub text: String,
}

/// What a repository has: what is told, and what of the kitchen it took away.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Instructions {
    /// The kitchen attached, as the configuration says it; `None`: the
    /// standard kitchen, which is Kito's rules alone.
    pub kitchen: Option<String>,
    /// `false` when the kitchen named is not there.
    pub kitchen_found: bool,
    pub items: Vec<Instruction>,
    /// The kitchen's instructions the repository took away.
    pub removed: Vec<String>,
}

/// `style`, `all/style` or `all/style.md` as the name of an instruction:
/// `all/style.md`. `Err`: what is wrong with it.
pub fn name_of(given: &str) -> Result<String, String> {
    let given = given.trim().trim_start_matches('/');
    let (folder, file) = match given.split_once('/') {
        Some((folder, file)) => (folder, file),
        None => (ALL, given),
    };
    let file = file.strip_suffix(".md").unwrap_or(file);
    let plain = |part: &str, dots: bool| {
        !part.is_empty()
            && !part.starts_with('.')
            && part.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || (dots && c == '.')
            })
    };
    if !plain(folder, false) || !plain(file, true) || file.contains('/') {
        return Err(format!(
            "`{given}` is not the name of an instruction: `<folder>/<file>`, in lower case letters, digits, \
             `-` and `_` (the folder `all` for every assistant, or the name of one: `claude/tests`)"
        ));
    }
    Ok(format!("{folder}/{file}.md"))
}

/// The folder of an instruction: `all`, `claude`, …
pub fn folder_of(name: &str) -> &str {
    name.split_once('/').map(|(folder, _)| folder).unwrap_or(ALL)
}

/// The instructions in a directory of that layout, by name.
fn read_dir(dir: &Path) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    let Ok(folders) = std::fs::read_dir(dir) else { return found };
    for folder in folders.flatten() {
        let Ok(files) = std::fs::read_dir(folder.path()) else { continue };
        let folder_name = folder.file_name().to_string_lossy().into_owned();
        for file in files.flatten() {
            let file_name = file.file_name().to_string_lossy().into_owned();
            let Ok(name) = name_of(&format!("{folder_name}/{file_name}")) else { continue };
            if !file_name.ends_with(".md") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(file.path()) {
                found.insert(name, text);
            }
        }
    }
    found
}

/// Where the kitchen named in the configuration is: from the root of the
/// repository when it is not absolute.
pub fn kitchen_dir(root: &Path, kitchen: &str) -> PathBuf {
    let path = Path::new(kitchen);
    match path.is_absolute() {
        true => path.to_owned(),
        false => root.join(path),
    }
}

fn own_dir(root: &Path) -> PathBuf {
    root.join(kito_store::STORE_DIR).join(DIR)
}

/// What the repository in `root` tells its agents.
pub fn load(root: &Path, kitchen: Option<&str>) -> Instructions {
    let kitchen_dir = kitchen.map(|kitchen| kitchen_dir(root, kitchen));
    let kitchen_found = kitchen_dir.as_ref().is_some_and(|dir| dir.is_dir());
    let of_kitchen = kitchen_dir.map(|dir| read_dir(&dir.join(DIR))).unwrap_or_default();
    let own = read_dir(&own_dir(root));
    let mut items = Vec::new();
    let mut removed = Vec::new();
    for (name, text) in &of_kitchen {
        match own.get(name) {
            Some(mine) if mine.trim().is_empty() => removed.push(name.clone()),
            Some(_) => {}
            None => items.push(Instruction {
                name: name.clone(),
                from: "kitchen",
                overrides: false,
                text: text.clone(),
            }),
        }
    }
    for (name, text) in &own {
        if text.trim().is_empty() {
            continue;
        }
        let overrides = of_kitchen.contains_key(name);
        items.push(Instruction { name: name.clone(), from: "repository", overrides, text: text.clone() });
    }
    items.sort_by(|a, b| order(&a.name).cmp(&order(&b.name)));
    Instructions { kitchen: kitchen.map(str::to_owned), kitchen_found, items, removed }
}

/// `all` first, then the folders of each assistant, each by file name.
fn order(name: &str) -> (bool, &str, &str) {
    let folder = folder_of(name);
    (folder != ALL, folder, name)
}

/// Sets one of the repository's own instructions. An empty text takes the
/// kitchen's of that name away. Answers whether anything changed.
pub fn set(root: &Path, name: &str, text: &str) -> Result<bool, String> {
    let name = name_of(name)?;
    let path = own_dir(root).join(&name);
    let text = match text.trim().is_empty() {
        true => String::new(),
        false => format!("{}\n", text.trim_end()),
    };
    if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    let said = |e: std::io::Error| format!("{}: {e}", path.display());
    std::fs::create_dir_all(path.parent().unwrap_or(&path)).map_err(said)?;
    let staged = path.with_extension("md.new");
    std::fs::write(&staged, text).and_then(|()| std::fs::rename(&staged, &path)).map_err(said)?;
    Ok(true)
}

/// Removes one of the repository's own instructions: the kitchen's of that
/// name, if there is one, is told again. Answers whether there was one.
pub fn remove(root: &Path, name: &str) -> Result<bool, String> {
    let name = name_of(name)?;
    let path = own_dir(root).join(&name);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            // An empty folder is not kept.
            let _ = path.parent().map(std::fs::remove_dir);
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_one_folder_and_one_file() {
        assert_eq!(name_of("style").unwrap(), "all/style.md");
        assert_eq!(name_of("claude/tests.md").unwrap(), "claude/tests.md");
        assert_eq!(name_of("/codex/v1.2").unwrap(), "codex/v1.2.md");
        for wrong in ["", "../x", "all/../x", "All/x", "a/b/c", ".hidden/x", "all/.x", "all/x y"] {
            assert!(name_of(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn the_repository_overrides_the_kitchen_and_an_empty_file_takes_one_away() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let kitchen = dir.path().join("kitchen");
        std::fs::create_dir_all(root.join(".kito")).unwrap();
        for (name, text) in
            [("all/style.md", "Tabs."), ("all/tests.md", "Run them."), ("claude/x.md", "Be brief.")]
        {
            let path = kitchen.join(DIR).join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        assert!(set(&root, "style", "Spaces.").unwrap());
        assert!(!set(&root, "all/style.md", "Spaces.\n").unwrap());
        assert!(set(&root, "tests", "").unwrap());
        assert!(set(&root, "codex/y", "Plan first.").unwrap());
        let got = load(&root, Some("../kitchen"));
        assert!(got.kitchen_found);
        let seen: Vec<(&str, &str, bool, &str)> =
            got.items.iter().map(|i| (i.name.as_str(), i.from, i.overrides, i.text.trim())).collect();
        assert_eq!(
            seen,
            [
                ("all/style.md", "repository", true, "Spaces."),
                ("claude/x.md", "kitchen", false, "Be brief."),
                ("codex/y.md", "repository", false, "Plan first."),
            ]
        );
        assert_eq!(got.removed, ["all/tests.md"]);
        assert!(remove(&root, "tests").unwrap());
        assert!(!remove(&root, "tests").unwrap());
        assert_eq!(load(&root, Some("../kitchen")).items.len(), 4);
        let none = load(&root, None);
        assert_eq!(none.items.len(), 2);
        assert!(!load(&root, Some("../nowhere")).kitchen_found);
    }
}
