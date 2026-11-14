//! A small repository in memory (and, for the language servers, on disk).
#![allow(dead_code)]

use kito_core::*;
use kito_diff::ast::{diff_symbols, FileChange, Version};
use kito_parse::{dialect_for_path, parse};
use kito_semantic::{Snapshot, Usage};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct World {
    pub root: PathBuf,
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    pub tree: Tree,
    pub table: SymbolTable,
    /// What differs from the files under `root`.
    pub overlay: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl World {
    /// A state with these files, recorded as a first capture would.
    pub fn new(files: &[(&str, &str)]) -> World {
        let mut world = World {
            root: PathBuf::from("/nowhere"),
            files: BTreeMap::new(),
            tree: Tree::new(),
            table: SymbolTable::new(),
            overlay: BTreeMap::new(),
        };
        for (path, content) in files {
            world.files.insert(PathBuf::from(path), content.as_bytes().to_vec());
        }
        for (path, content) in &world.files {
            let add =
                OpKind::FileAdd { path: path.clone(), blob: BlobId::of(content), mode: FileMode::Regular };
            world.tree.apply(&add).unwrap();
            let Some(parsed) = dialect_for_path(path).and_then(|dialect| parse(dialect, content)) else {
                continue;
            };
            let change = FileChange { path, old: None, new: Some(Version { content, parsed: &parsed }) };
            for op in diff_symbols(&world.table, LaneId::MAIN, &[change]).ops {
                world.table.apply(&op).unwrap();
            }
        }
        world
    }

    /// The same, with the files also written under `root`.
    pub fn on_disk(root: &Path, files: &[(&str, &str)]) -> World {
        let mut world = World::new(files);
        world.root = root.to_owned();
        for (path, content) in &world.files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
        world
    }

    /// Another state of the same repository: these files changed (or, with
    /// `None`, removed) without touching the disk, as in a lane.
    pub fn with(&self, changes: &[(&str, Option<&str>)]) -> World {
        let mut files: Vec<(String, String)> = self
            .files
            .iter()
            .map(|(p, c)| (p.to_str().unwrap().to_owned(), String::from_utf8(c.clone()).unwrap()))
            .collect();
        let mut overlay = self.overlay.clone();
        for (path, content) in changes {
            files.retain(|(p, _)| p != path);
            if let Some(content) = content {
                files.push(((*path).to_owned(), (*content).to_owned()));
            }
            overlay.insert(PathBuf::from(path), content.map(|c| c.as_bytes().to_vec()));
        }
        let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect();
        let mut world = World::new(&files);
        world.root = self.root.clone();
        world.overlay = overlay;
        world
    }

    pub fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            root: &self.root,
            tree: &self.tree,
            symbols: &self.table,
            files: self,
            overlay: &self.overlay,
        }
    }

    pub fn id(&self, file: &str, name: &str) -> SymbolId {
        match self.table.find(Path::new(file), name).as_slice() {
            [id] => *id,
            other => panic!("{name} in {file}: {other:?}"),
        }
    }

    /// One line per use: where, the line it is on, how sure.
    pub fn show(&self, usages: &[Usage]) -> Vec<String> {
        usages
            .iter()
            .map(|usage| {
                let content = std::str::from_utf8(&self.files[&usage.file]).unwrap();
                let line = content[..usage.range.start].matches('\n').count();
                let text = content.lines().nth(line).unwrap().trim();
                format!("{}:{} `{text}` {:.1}", usage.file.display(), line + 1, usage.confidence)
            })
            .collect()
    }
}

impl kito_semantic::Files for World {
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        self.files.get(path).cloned()
    }
}
