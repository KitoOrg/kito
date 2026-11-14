//! Three states of a repository and the captures that lead to them, without
//! a store: enough to exercise the merge.
#![allow(dead_code)]

use kito_core::*;
use kito_diff::ast::{diff_symbols_with, FileChange, Identities, Version};
use kito_merge::{merge, Choice, Input, Outcome, Side, Subject};
use kito_parse::{dialect_for_path, parse, ParsedFile};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub type Blobs = HashMap<BlobId, Vec<u8>>;

/// The state of one lane: its files and what its history says of their symbols.
#[derive(Clone)]
pub struct State {
    pub lane: LaneId,
    pub tree: Tree,
    pub table: SymbolTable,
    /// IDs of the symbols this lane creates start here.
    pub next_symbol: u64,
}

fn parsed(path: &Path, content: &[u8]) -> Option<ParsedFile> {
    parse(dialect_for_path(path)?, content)
}

impl State {
    pub fn new(lane: LaneId) -> State {
        State { lane, tree: Tree::new(), table: SymbolTable::new(), next_symbol: 0 }
    }

    pub fn read(&self, blobs: &Blobs, path: &str) -> Option<String> {
        let entry = self.tree.get(Path::new(path))?;
        Some(String::from_utf8(blobs[&entry.blob].clone()).unwrap())
    }

    pub fn id(&self, file: &str, name: &str) -> SymbolId {
        match self.table.find(Path::new(file), name).as_slice() {
            [id] => *id,
            other => panic!("{name} in {file}: {other:?}"),
        }
    }

    /// Saves files as an editor would (`None` removes one): the capture
    /// records the file-level change and what it means for the symbols.
    pub fn write(&mut self, blobs: &mut Blobs, changes: &[(&str, Option<&str>)]) {
        let changes: BTreeMap<PathBuf, Option<(Vec<u8>, FileMode)>> = changes
            .iter()
            .map(|(p, c)| (PathBuf::from(p), c.map(|c| (c.as_bytes().to_vec(), FileMode::Regular))))
            .collect();
        self.record(blobs, &changes, &Identities::new());
    }

    /// Records a new content for some files, with identities known in advance.
    pub fn record(
        &mut self,
        blobs: &mut Blobs,
        changes: &BTreeMap<PathBuf, Option<(Vec<u8>, FileMode)>>,
        identities: &Identities,
    ) {
        struct Change {
            path: PathBuf,
            old: Option<(Vec<u8>, Option<ParsedFile>)>,
            new: Option<(Vec<u8>, Option<ParsedFile>)>,
        }
        let mut loaded = Vec::new();
        for (path, new) in changes {
            let old = self.tree.get(path).map(|e| blobs[&e.blob].clone());
            let op = match (&old, new) {
                (None, Some((content, mode))) => {
                    OpKind::FileAdd { path: path.clone(), blob: BlobId::of(content), mode: *mode }
                }
                (Some(_), None) => OpKind::FileRemove { path: path.clone() },
                (Some(before), Some((after, _))) if before != after => OpKind::TextEdit {
                    file: path.clone(),
                    before: BlobId::of(before),
                    after: BlobId::of(after),
                    edits: Vec::new(),
                },
                _ => continue,
            };
            if let Some((content, _)) = new {
                blobs.insert(BlobId::of(content), content.clone());
            }
            self.tree.apply(&op).unwrap();
            let with_parse = |c: Vec<u8>| {
                let file = parsed(path, &c);
                (c, file)
            };
            loaded.push(Change {
                path: path.clone(),
                old: old.map(with_parse),
                new: new.as_ref().map(|(c, _)| with_parse(c.clone())),
            });
        }
        fn version(side: &Option<(Vec<u8>, Option<ParsedFile>)>) -> Option<Version<'_>> {
            let (content, parsed) = side.as_ref()?;
            Some(Version { content, parsed: parsed.as_ref()? })
        }
        let file_changes: Vec<FileChange<'_>> = loaded
            .iter()
            .filter(|c| dialect_for_path(&c.path).is_some())
            .map(|c| FileChange { path: &c.path, old: version(&c.old), new: version(&c.new) })
            .collect();
        let mut identities = identities.clone();
        identities.start_new_ids_at(self.next_symbol);
        let result = diff_symbols_with(&self.table, self.lane, &file_changes, &identities);
        for op in &result.ops {
            self.table.apply(op).unwrap_or_else(|e| panic!("{op:?} does not apply: {e}"));
        }
        self.next_symbol = self.next_symbol.max(self.table.next_seq(self.lane));
        self.assert_table_mirrors_files(blobs);
    }

    /// The table lists exactly the symbols the parser sees in the files.
    pub fn assert_table_mirrors_files(&self, blobs: &Blobs) {
        for (path, entry) in self.tree.iter() {
            let Some(file) = parsed(path, &blobs[&entry.blob]) else { continue };
            let mut seen: Vec<(String, SymbolKind)> =
                (0..file.symbols.len()).map(|i| (file.qualified_name(i), file.symbols[i].kind)).collect();
            let mut known: Vec<(String, SymbolKind)> = self
                .table
                .in_file(path)
                .map(|(id, e)| (self.table.qualified_name(id).unwrap(), e.kind))
                .collect();
            seen.sort();
            known.sort();
            assert_eq!(known, seen, "{} on {}", path.display(), self.lane);
        }
    }
}

/// A repository with a main lane and some lanes that started from it.
pub struct World {
    pub blobs: Blobs,
    pub main: State,
    /// Per lane: its state and the state of the main lane it started from.
    pub lanes: Vec<(State, State)>,
}

impl World {
    /// A main lane with these files.
    pub fn new(files: &[(&str, &str)]) -> World {
        let mut blobs = Blobs::new();
        let mut main = State::new(LaneId::MAIN);
        let files: Vec<(&str, Option<&str>)> = files.iter().map(|(p, c)| (*p, Some(*c))).collect();
        main.write(&mut blobs, &files);
        World { blobs, main, lanes: Vec::new() }
    }

    /// Opens a lane on the current state of the main lane. Returns its index.
    pub fn open(&mut self) -> usize {
        let mut lane = self.main.clone();
        lane.lane = LaneId(self.lanes.len() as u64 + 1);
        lane.next_symbol = 0;
        self.lanes.push((lane, self.main.clone()));
        self.lanes.len() - 1
    }

    pub fn on_lane(&mut self, lane: usize, changes: &[(&str, Option<&str>)]) {
        self.lanes[lane].0.write(&mut self.blobs, changes);
    }

    pub fn on_main(&mut self, changes: &[(&str, Option<&str>)]) {
        self.main.write(&mut self.blobs, changes);
    }

    /// What integrating the lane would give, with these decisions.
    pub fn merge_with(&self, lane: usize, decisions: &[(Subject, Choice)]) -> Outcome {
        let (state, base) = &self.lanes[lane];
        let resolutions: HashMap<Subject, Choice> = decisions.iter().cloned().collect();
        let blobs = &self.blobs;
        merge(&Input {
            base: Side { tree: &base.tree, symbols: &base.table },
            main: Side { tree: &self.main.tree, symbols: &self.main.table },
            lane: Side { tree: &state.tree, symbols: &state.table },
            blob: &|id| blobs.get(id).cloned(),
            resolutions: &resolutions,
        })
    }

    pub fn merge(&self, lane: usize) -> Outcome {
        self.merge_with(lane, &[])
    }

    /// Integrates the lane: the merged files are recorded on the main lane.
    /// Panics on a conflict.
    pub fn integrate(&mut self, lane: usize) -> Outcome {
        self.integrate_with(lane, &[])
    }

    pub fn integrate_with(&mut self, lane: usize, decisions: &[(Subject, Choice)]) -> Outcome {
        let outcome = self.merge_with(lane, decisions);
        assert!(outcome.conflicts.is_empty(), "conflicts: {:#?}", describe(&outcome));
        let changes: BTreeMap<PathBuf, Option<(Vec<u8>, FileMode)>> = outcome
            .files
            .iter()
            .map(|(p, f)| (p.clone(), f.as_ref().map(|f| (f.content.clone(), f.mode))))
            .collect();
        self.main.record(&mut self.blobs, &changes, &outcome.identities);
        // The lane continues from the new state of the main lane.
        let id = self.lanes[lane].0.lane;
        let next_symbol = self.lanes[lane].0.next_symbol;
        let mut state = self.main.clone();
        state.lane = id;
        state.next_symbol = next_symbol;
        self.lanes[lane] = (state, self.main.clone());
        outcome
    }

    pub fn main_file(&self, path: &str) -> String {
        self.main.read(&self.blobs, path).unwrap_or_else(|| panic!("{path} is not on the main lane"))
    }

    /// Every file of the main lane, for comparisons.
    pub fn main_files(&self) -> BTreeMap<String, String> {
        self.main
            .tree
            .iter()
            .map(|(p, e)| {
                (p.display().to_string(), String::from_utf8_lossy(&self.blobs[&e.blob]).into_owned())
            })
            .collect()
    }
}

/// The conflicts of an outcome, one line each.
pub fn describe(outcome: &Outcome) -> Vec<String> {
    outcome
        .conflicts
        .iter()
        .map(|c| {
            let what = match (&c.name, &c.subject) {
                (Some(name), _) => name.clone(),
                (None, Subject::File(path)) => path.display().to_string(),
                (None, Subject::Symbol(id)) => id.to_string(),
            };
            format!("{:?} {what} in {}", c.kind, c.file.display())
        })
        .collect()
}
