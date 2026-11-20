//! Notes on code (ADR 0003): a reason that belongs to no change, left on a
//! function, a file or a directory, and told with the why of that code.

use crate::error::{Error, Result};
use crate::why::{parse_target, Asked};
use crate::Repo;
use kito_core::{Author, LaneId, Note, NoteId, NoteTarget, SymbolId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A note as the clients see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteInfo {
    /// `note#3`.
    pub id: String,
    /// What it is on, as it is called now: `src/shop.ts:total`,
    /// `src/shop.ts`, `src/billing/`.
    pub on: String,
    pub text: String,
    pub by: String,
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// The function it is on no longer exists.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gone: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

impl Repo {
    /// What a note on `target` is on, as `lane` has the code: a function
    /// (`file:name`), a file, a directory (`dir/`), or lines, which are the
    /// innermost function they are in, else the file.
    fn note_target(&self, lane: LaneId, target: &str) -> Result<NoteTarget> {
        let text = kito_core::paths::portable_text(target);
        let text = text.trim();
        let work = self.work(lane)?;
        let under = |dir: &Path| work.tree.iter().any(|(path, _)| path.starts_with(dir));
        if let Some(dir) = text.strip_suffix('/').filter(|dir| !dir.is_empty()) {
            let dir = crate::scope::clean(dir)?;
            return match under(&dir) {
                true => Ok(NoteTarget::Path(dir)),
                false => Err(Error::Other(format!("{}/: no file is under it", dir.display()))),
            };
        }
        let symbol = |id: SymbolId| {
            let entry = work.symbols.get(id)?;
            let name = work.symbols.qualified_name(id).unwrap_or_else(|| entry.name.clone());
            Some(NoteTarget::Symbol { id, file: entry.file.clone(), name })
        };
        match parse_target(text)? {
            Asked::Symbol(file, name) => match work.symbols.find(&file, &name).as_slice() {
                [id] => symbol(*id).ok_or_else(|| Error::Other(format!("{text}: no such function"))),
                [] if work.tree.contains(Path::new(text)) => Ok(NoteTarget::Path(PathBuf::from(text))),
                [] => Err(Error::Other(format!("{}: nothing called `{name}`", file.display()))),
                several => Err(Error::Other(format!(
                    "{} things are called `{name}` in {}: leave the note on one by its lines ({}:<line>)",
                    several.len(),
                    file.display(),
                    file.display()
                ))),
            },
            Asked::File(path) => {
                let path = crate::scope::clean(&path.to_string_lossy())?;
                match (work.tree.contains(&path), under(&path)) {
                    (true, _) | (_, true) => Ok(NoteTarget::Path(path)),
                    _ => Err(Error::Other(format!("{}: no such file", path.display()))),
                }
            }
            Asked::Lines(file, from, _) => {
                let spans = self.symbol_spans(lane, &file)?;
                match spans.iter().rfind(|(_, start, end)| *start <= from && from <= *end) {
                    Some((id, _, _)) => {
                        symbol(*id).ok_or_else(|| Error::Other(format!("{text}: no such function")))
                    }
                    None if work.tree.contains(&file) => Ok(NoteTarget::Path(file)),
                    None => Err(Error::Other(format!("{}: no such file", file.display()))),
                }
            }
        }
    }

    /// Leaves a note on code. `by`: who writes it (default: whoever runs
    /// the repository); `person`: whom an agent works for.
    pub fn add_note(
        &mut self,
        lane: LaneId,
        target: &str,
        text: &str,
        by: Option<Author>,
        person: Option<Author>,
    ) -> Result<NoteInfo> {
        let text = text.trim();
        if text.chars().filter(|c| c.is_alphabetic()).count() < 3 {
            return Err(Error::Other(
                "a note needs a few words: what is there to know about this code".into(),
            ));
        }
        // A note on a lane's code is on the code as its agent just left it.
        if !lane.is_main() {
            self.scan_lane(lane)?;
        }
        let on = self.note_target(lane, target)?;
        let note = Note {
            id: self.store.next_note_id(),
            on,
            text: text.to_owned(),
            by: by.unwrap_or_else(|| self.identity.author()),
            person,
            at_ms: now_ms(),
            lane: (!lane.is_main()).then_some(lane),
            removed: None,
        };
        self.store.put_note(&note)?;
        Ok(self.note_info(lane, &note))
    }

    /// The notes not removed; with `target`, those on it, on its file and
    /// on the directories above it.
    pub fn notes(&self, lane: LaneId, target: Option<&str>) -> Result<Vec<NoteInfo>> {
        let Some(target) = target else {
            let all = self.store.notes().filter(|note| note.removed.is_none());
            return Ok(all.map(|note| self.note_info(lane, note)).collect());
        };
        Ok(match self.note_target(lane, target)? {
            NoteTarget::Symbol { id, file, .. } => self.notes_for(lane, Some(id), &file),
            NoteTarget::Path(path) => self.notes_for(lane, None, &path),
        })
    }

    /// Removes a note: it is kept, marked with who removed it and when.
    pub fn remove_note(&mut self, id: NoteId, by: Option<Author>) -> Result<NoteInfo> {
        let mut note =
            self.store.note(id).cloned().ok_or_else(|| Error::Other(format!("{id}: no such note")))?;
        if note.removed.is_some() {
            return Err(Error::Other(format!("{id} was already removed")));
        }
        note.removed = Some((by.unwrap_or_else(|| self.identity.author()), now_ms()));
        self.store.put_note(&note)?;
        Ok(self.note_info(LaneId::MAIN, &note))
    }

    /// The notes told with the why of a function (`symbol`, in `file`) or
    /// of a file: those on it, on its file, on the directories above; for
    /// a file, also those on the functions in it.
    pub(crate) fn notes_for(&self, lane: LaneId, symbol: Option<SymbolId>, file: &Path) -> Vec<NoteInfo> {
        let Ok(work) = self.work(lane) else { return Vec::new() };
        self.store
            .notes()
            .filter(|note| note.removed.is_none())
            .filter(|note| match &note.on {
                NoteTarget::Symbol { id, .. } => match symbol {
                    Some(asked) => *id == asked,
                    None => work.symbols.get(*id).is_some_and(|entry| entry.file == file),
                },
                NoteTarget::Path(path) => file == path || file.starts_with(path),
            })
            .map(|note| self.note_info(lane, note))
            .collect()
    }

    /// The notes on lines of a file about to be edited: those on the
    /// functions the lines are in, on the file and the directories above.
    /// `None`: the whole file, and the notes of every function in it.
    pub(crate) fn notes_on_lines(
        &self,
        lane: LaneId,
        file: &Path,
        lines: Option<&[(u32, u32)]>,
    ) -> Vec<NoteInfo> {
        let Some(lines) = lines else { return self.notes_for(lane, None, file) };
        let spans = self.symbol_spans(lane, file).unwrap_or_default();
        let mut found: Vec<NoteInfo> = Vec::new();
        let mut functions = 0;
        for (id, start, end) in &spans {
            if lines.iter().any(|(from, to)| from <= end && start <= to) {
                functions += 1;
                found.extend(self.notes_for(lane, Some(*id), file));
            }
        }
        if functions == 0 {
            // Outside every function: the file's own notes.
            let all = self.notes_for(lane, None, file);
            found.extend(all.into_iter().filter(|note| !note.on.contains(':')));
        }
        found.sort_by(|a, b| a.id.cmp(&b.id));
        found.dedup_by(|a, b| a.id == b.id);
        found
    }

    fn note_info(&self, lane: LaneId, note: &Note) -> NoteInfo {
        let work = self.work(lane).ok();
        let (on, gone) = match &note.on {
            NoteTarget::Symbol { id, file, name } => {
                let now = work.and_then(|work| {
                    let entry = work.symbols.get(*id)?;
                    let name = work.symbols.qualified_name(*id).unwrap_or_else(|| entry.name.clone());
                    Some(format!("{}:{name}", entry.file.display()))
                });
                match now {
                    Some(now) => (now, false),
                    None => (format!("{}:{name}", file.display()), true),
                }
            }
            NoteTarget::Path(path) => {
                let directory = work.is_some_and(|work| !work.tree.contains(path));
                let slash = if directory { "/" } else { "" };
                (format!("{}{slash}", path.display()), false)
            }
        };
        NoteInfo {
            id: note.id.to_string(),
            on,
            text: note.text.clone(),
            by: note.by.name.clone(),
            person: note.person.as_ref().map(|person| person.name.clone()),
            at_ms: note.at_ms,
            lane: note.lane.map(|lane| lane.to_string()),
            gone,
        }
    }
}
