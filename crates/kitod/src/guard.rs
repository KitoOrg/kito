//! What an agent is held to, asked before the fact.
//!
//! The rules of a repository (work in a lane, change what you declared,
//! know why code is the way it is before you change it) are checked when a
//! lane is integrated. That is late: the work is done by then. This module
//! answers the same questions at the moment they matter:
//!
//! - when a lane declares what it will change, it is told why that code is
//!   the way it is ([`Repo::why_of_scope`]);
//! - before an edit, whether it may be made at all: in a lane, inside what
//!   the lane declared, and with the reasons behind the lines it replaces
//!   already told ([`Repo::before_edit`]). An edit on lines written for a
//!   reason the lane was not told is refused once, with the reasons: made
//!   again, it goes through;
//! - before an integration, the reasons behind what the lane replaced that
//!   it was never told ([`Repo::reasons_behind`]): for agents with no hooks
//!   and for changes made through a shell;
//! - after a shell command, what it changed where it should not have
//!   ([`Repo::after_shell`]).
//!
//! What a lane was told is remembered with the lane, so nothing is told
//! twice, a restart of the daemon included.

use crate::error::{Error, Result};
use crate::repo::Repo;
use crate::why::{Decision, Reason};
use kito_core::{LaneId, ScopeItem};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// How many declared items are told about in one answer, how many reasons
/// of each, how many users of each, and how many reasons one refusal lists.
const ITEMS_TOLD: usize = 8;
const REASONS_PER_ITEM: usize = 5;
const USERS_PER_ITEM: usize = 8;
const REASONS_PER_REFUSAL: usize = 8;

/// How many direct edits of the main directory are remembered for whoever
/// asks what a command did.
const DIRECT_KEPT: usize = 64;

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Why something a lane declared is the way it is, in short.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeWhy {
    /// What was declared: `src/shop.py:total`, `src/billing.py`.
    pub item: String,
    /// The reasons it changed for, newest first.
    pub reasons: Vec<Reason>,
    /// How many older reasons are not listed: `why` tells them all.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub earlier: usize,
    /// Who uses it, as far as the syntax tells: `src/billing.py:invoice (line 5)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_by: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub used_by_more: usize,
    /// What was decided about it when two lanes disagreed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
    /// The notes left on it (ADR 0003).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<crate::notes::NoteInfo>,
}

/// The answer to "may this edit be made?".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditAnswer {
    pub allowed: bool,
    /// `ok`; or why not: `main` (the main directory), `closed` (no such
    /// open lane), `out_of_scope`, `comments` (what was said about the lane, to read
    /// first), `why` (reasons to read first).
    pub kind: String,
    /// What to tell whoever wanted to make the edit.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// What is outside the scope of the lane.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outside: Vec<String>,
    /// The reasons behind the lines the edit replaces, not told before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<Reason>,
    /// The notes left on that code, not told before (ADR 0003).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<crate::notes::NoteInfo>,
    /// What was said about the lane, not told before (ADR 0004).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<crate::comments::CommentInfo>,
}

/// A reason behind code a lane replaced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replaced {
    #[serde(flatten)]
    pub reason: Reason,
    /// The files where the lane replaced lines written for it.
    pub replaced_in: Vec<String>,
}

/// The reasons behind what a lane replaced that it was not told before.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Behind {
    pub reasons: Vec<Replaced>,
    /// How many more there are, older: `why` tells them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub more: usize,
    /// The same, to be read.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// Files of the main directory changed by hand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Direct {
    pub at_ms: u64,
    pub paths: Vec<String>,
    /// The main lane is protected: the files were put back.
    pub put_back: bool,
    /// Where what was written is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<PathBuf>,
}

/// A lane that changed something outside what it declared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneOutside {
    pub lane: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    pub out_of_scope: Vec<String>,
}

/// What a command changed where it should not have.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellReport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub main: Vec<Direct>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<LaneOutside>,
    /// The same, to be read; empty when there is nothing to tell.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// What tells one reason from another for somebody who was told: the lane
/// and its words, or, for what came to the main lane by hand or from git,
/// the checkpoint that took it in.
fn told_key(reason: &Reason) -> String {
    match &reason.lane {
        Some(lane) => format!("{lane}|{}", reason.why),
        None => format!("main|{}|{}", reason.checkpoint.as_deref().unwrap_or(""), reason.why),
    }
}

/// A reason in one line: `"prices in cents" (task #7): agent-a for ada, lane/1, checkpoint 3f2a91c0`.
pub fn line_of(reason: &Reason) -> String {
    let mut line = format!("\"{}\"", reason.why);
    if let Some(task) = &reason.task {
        line.push_str(&format!(" ({task})"));
    }
    line.push_str(&format!(": {}", reason.by));
    if let Some(person) = reason.person.as_ref().filter(|person| **person != reason.by) {
        line.push_str(&format!(" for {person}"));
    }
    if let Some(lane) = &reason.lane {
        line.push_str(&format!(", {lane}"));
    }
    match &reason.checkpoint {
        Some(checkpoint) => {
            line.push_str(&format!(", checkpoint {}", checkpoint.chars().take(8).collect::<String>()))
        }
        None => line.push_str(", not integrated yet"),
    }
    if let Some(person) = &reason.approved_by {
        line.push_str(&format!(", let in by {person}"));
    }
    if !reason.reviewed_by.is_empty() {
        line.push_str(&format!(", reviewed by {}", reason.reviewed_by.join(", ")));
    }
    line
}

/// A reason without what only `why` needs to tell.
fn short(mut reason: Reason) -> Reason {
    reason.ops.clear();
    if reason.with.len() > 4 {
        reason.with_more += reason.with.len() - 4;
        reason.with.truncate(4);
    }
    reason
}

fn list(items: &[String]) -> String {
    const SHOWN: usize = 6;
    match items.len() > SHOWN {
        true => format!("{} and {} more", items[..SHOWN].join(", "), items.len() - SHOWN),
        false => items.join(", "),
    }
}

impl Repo {
    fn told_file(&self, lane: LaneId) -> PathBuf {
        self.lane_dir(lane).join("told.json")
    }

    /// What a lane was told so far, read from where it is kept the first time.
    fn told_reasons(&mut self, lane: LaneId) -> &mut BTreeSet<String> {
        if !self.reasons_told.contains_key(&lane) {
            let saved: BTreeSet<String> = std::fs::read(self.told_file(lane))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            self.reasons_told.insert(lane, saved);
        }
        self.reasons_told.entry(lane).or_default()
    }

    /// Of some reasons, the ones a lane has not been told and are not its own.
    fn untold(&mut self, lane: LaneId, reasons: Vec<Reason>) -> Vec<Reason> {
        let own = lane.to_string();
        let told = self.told_reasons(lane);
        let mut seen = BTreeSet::new();
        reasons
            .into_iter()
            .filter(|reason| reason.lane.as_deref() != Some(own.as_str()))
            .filter(|reason| !told.contains(&told_key(reason)))
            .filter(|reason| seen.insert(told_key(reason)))
            .collect()
    }

    /// Of some notes, the ones a lane has not been told (ADR 0003).
    fn untold_notes(
        &mut self,
        lane: LaneId,
        notes: Vec<crate::notes::NoteInfo>,
    ) -> Vec<crate::notes::NoteInfo> {
        let told = self.told_reasons(lane);
        let mut seen = BTreeSet::new();
        notes.into_iter().filter(|note| !told.contains(&note.id) && seen.insert(note.id.clone())).collect()
    }

    /// The comments on a lane its agent was not told (ADR 0004).
    pub(crate) fn untold_comments(&mut self, lane: LaneId) -> Vec<crate::comments::CommentInfo> {
        let comments = self.comments_for_agent(lane);
        if comments.is_empty() {
            return comments;
        }
        let told = self.told_reasons(lane);
        comments.into_iter().filter(|comment| !told.contains(&comment.id)).collect()
    }

    /// Remembers that the agent of a lane was told these comments.
    pub(crate) fn tell_comments(&mut self, lane: LaneId, comments: &[crate::comments::CommentInfo]) {
        if comments.is_empty() || !self.lanes.contains_key(&lane) {
            return;
        }
        let told = self.told_reasons(lane);
        told.extend(comments.iter().map(|comment| comment.id.clone()));
        if let Ok(bytes) = serde_json::to_vec(told) {
            let _ = std::fs::write(self.told_file(lane), bytes);
        }
    }

    /// Remembers that a lane was told these notes, with the reasons.
    pub(crate) fn tell_notes(&mut self, lane: LaneId, notes: &[crate::notes::NoteInfo]) {
        if notes.is_empty() || !self.lanes.contains_key(&lane) {
            return;
        }
        let told = self.told_reasons(lane);
        told.extend(notes.iter().map(|note| note.id.clone()));
        if let Ok(bytes) = serde_json::to_vec(told) {
            let _ = std::fs::write(self.told_file(lane), bytes);
        }
    }

    /// Remembers that a lane was told these reasons: by an answer of this
    /// module, or because its agent asked why by itself.
    pub fn tell(&mut self, lane: LaneId, reasons: &[Reason]) {
        if reasons.is_empty() || !self.lanes.contains_key(&lane) {
            return;
        }
        let told = self.told_reasons(lane);
        let before = told.len();
        told.extend(reasons.iter().map(told_key));
        if told.len() != before {
            // Kept for a restart; if it cannot be written, the lane is told again.
            if let Ok(bytes) = serde_json::to_vec(told) {
                let _ = std::fs::write(self.told_file(lane), bytes);
            }
        }
    }

    /// Why what a lane just declared is the way it is: for each function
    /// and file that exists, the last reasons it changed for and who uses
    /// it. The lane counts as told. A note says what is left untold.
    pub fn why_of_scope(&mut self, lane: LaneId, declared: &[String]) -> (Vec<ScopeWhy>, Option<String>) {
        let mut targets: Vec<String> = Vec::new();
        let mut directories = 0;
        if let Ok(work) = self.work(lane) {
            for text in declared {
                match crate::scope::parse_item(&work.tree, &work.symbols, text) {
                    Ok(ScopeItem::Dir(_)) => directories += 1,
                    Ok(ScopeItem::File(file)) if work.tree.contains(&file) => {
                        targets.push(file.display().to_string())
                    }
                    Ok(item @ ScopeItem::Symbol { .. }) => targets.push(crate::scope::show(&item)),
                    _ => {}
                }
            }
        }
        targets.dedup();
        let more = targets.len().saturating_sub(ITEMS_TOLD);
        targets.truncate(ITEMS_TOLD);
        let own = lane.to_string();
        let mut told = Vec::new();
        for item in targets {
            // What does not exist yet has no past.
            let Ok(graph) = self.why_graph(lane, &item, false) else { continue };
            let mut reasons: Vec<Reason> = graph
                .reasons
                .into_iter()
                .filter(|reason| reason.lane.as_deref() != Some(own.as_str()))
                .collect();
            let earlier = reasons.len().saturating_sub(REASONS_PER_ITEM);
            reasons.truncate(REASONS_PER_ITEM);
            let reasons: Vec<Reason> = reasons.into_iter().map(short).collect();
            let used_by_more = graph.used_by.len().saturating_sub(USERS_PER_ITEM);
            let used_by: Vec<String> = graph
                .used_by
                .iter()
                .take(USERS_PER_ITEM)
                .map(|user| match user.line {
                    Some(line) => format!("{} (line {line})", user.symbol),
                    None => user.symbol.clone(),
                })
                .collect();
            let annotations = graph.annotations;
            if reasons.is_empty()
                && used_by.is_empty()
                && graph.decisions.is_empty()
                && annotations.is_empty()
            {
                continue;
            }
            self.tell(lane, &reasons);
            self.tell_notes(lane, &annotations);
            told.push(ScopeWhy {
                item,
                reasons,
                earlier,
                used_by,
                used_by_more,
                decisions: graph.decisions,
                annotations,
            });
        }
        let mut notes = Vec::new();
        if directories > 0 {
            notes.push(
                "a directory was declared: why a file in it is the way it is, is told when it is about to \
                 change, and by the `why` tool at any time"
                    .to_owned(),
            );
        }
        if more > 0 {
            notes.push(format!("{more} more declared items are not told about here: ask the `why` tool"));
        }
        (told, (!notes.is_empty()).then(|| notes.join("; ")))
    }

    /// May this edit be made? `lane`: the lane whose working copy the file
    /// is in; `None` for the main directory. `lines`: the lines of the
    /// file, as it is now, that the edit replaces; `whole`: all of them.
    pub fn before_edit(
        &mut self,
        lane: Option<LaneId>,
        path: &Path,
        lines: &[(u32, u32)],
        whole: bool,
    ) -> Result<EditAnswer> {
        let ok = EditAnswer { allowed: true, kind: "ok".to_owned(), ..EditAnswer::default() };
        let refused = |kind: &str, message: String| EditAnswer {
            allowed: false,
            kind: kind.to_owned(),
            message,
            ..EditAnswer::default()
        };
        let file = path.display();
        let Some(lane) = lane else {
            // What Kito does not record is nobody's business here.
            if self.main.ignore.is_ignored(path, false) && !self.main.tree.contains(path) {
                return Ok(ok);
            }
            let how = match self.config.main.protected {
                true => "the main lane is protected: the edit would be put back as soon as it is saved",
                false => "it is where finished work lands, not where it is made",
            };
            return Ok(refused(
                "main",
                format!(
                    "Kito: {file} is in the main directory, and agents do not edit there ({how}). Open a \
                     lane with the open_lane tool, saying what you will change and why (in a lane you \
                     already have, declare this file with extend_scope instead); the answer gives the \
                     working copy (`workdir`) and tells why the code you declared is the way it is. \
                     Then make this edit there: <workdir>/{file}."
                ),
            ));
        };
        if lane.is_main() || !self.lanes.contains_key(&lane) {
            return Ok(refused(
                "closed",
                format!(
                    "Kito: {lane} is not an open lane (it was closed, or never opened here), so nothing \
                     written in its working copy is recorded. Open a lane with the open_lane tool and \
                     edit in the working copy it gives."
                ),
            ));
        }
        self.scan_lane(lane)?;
        {
            let work = self.work(lane)?;
            if work.ignore.is_ignored(path, false) && !work.tree.contains(path) {
                return Ok(ok);
            }
        }

        // Inside what the lane declared?
        let outside = self.outside_scope(lane, path, if whole { &[] } else { lines })?;
        if !outside.is_empty() {
            let declared: Vec<String> = self.lane_scope(lane)?.into_iter().map(|item| item.item).collect();
            let declared = match declared.is_empty() {
                true => "nothing".to_owned(),
                false => list(&declared),
            };
            let scope: Vec<String> = outside.iter().map(|item| format!("\"{item}\"")).collect();
            return Ok(EditAnswer {
                outside: outside.clone(),
                ..refused(
                    "out_of_scope",
                    format!(
                        "Kito: {} is outside what {lane} declared ({declared}), so this edit was not \
                         made. Declare it first: call the extend_scope tool with lane \"{lane}\" and \
                         scope [{}]. Its answer tells why that code is the way it is and who else is on \
                         it. Then make this edit again.",
                        list(&outside),
                        scope.join(", "),
                    ),
                )
            });
        }

        // Told what was said about the lane (ADR 0004)? Whoever reviews it
        // speaks to the agent through its comments.
        let comments = self.untold_comments(lane);
        if !comments.is_empty() {
            self.tell_comments(lane, &comments);
            let mut message =
                format!("Kito: this edit was held back once. Somebody commented on {lane} and you have not read it:\n");
            for comment in &comments {
                let on = comment.on.as_ref().map(|on| format!(" on {on}")).unwrap_or_default();
                let to = comment.reply_to.as_ref().map(|to| format!(", answering {to}")).unwrap_or_default();
                let old = if comment.outdated { ", on code that changed since" } else { "" };
                message
                    .push_str(&format!("- {}{on} ({}{to}{old}): {}\n", comment.id, comment.by, comment.text));
                for line in comment.quoted.iter().flat_map(|quoted| quoted.lines()).take(6) {
                    message.push_str(&format!("    > {line}\n"));
                }
            }
            message.push_str(
                "Do what they ask, or answer with the comment tool (reply_to) when you will not or when \
                 there is something to say. Then make this edit again: it goes through.",
            );
            return Ok(EditAnswer { comments, ..refused("comments", message) });
        }

        // Told why the lines are the way they are?
        if !self.config.agents.why || !self.work(lane)?.tree.contains(path) {
            return Ok(ok);
        }
        let asked = match whole || lines.is_empty() {
            true => None,
            false => Some(lines),
        };
        if asked.is_none() && !whole {
            // Nothing is replaced: lines are only added.
            return Ok(ok);
        }
        let reasons = self.reasons_of_lines(lane, path, asked)?;
        let reasons: Vec<Reason> = self.untold(lane, reasons).into_iter().map(short).collect();
        let notes = self.notes_on_lines(lane, path, asked);
        let notes = self.untold_notes(lane, notes);
        if reasons.is_empty() && notes.is_empty() {
            return Ok(ok);
        }
        self.tell(lane, &reasons);
        self.tell_notes(lane, &notes);
        let what = match whole {
            true => format!("{file}"),
            false => format!("these lines of {file}"),
        };
        let mut message = format!("Kito: this edit was held back once. Before you change {what}:\n");
        if !notes.is_empty() {
            message.push_str("Notes left on this code:\n");
            for note in &notes {
                message.push_str(&format!("- {} on {} ({}): {}\n", note.id, note.on, note.by, note.text));
            }
        }
        if !reasons.is_empty() {
            message.push_str("This is why it is the way it is:\n");
        }
        for reason in reasons.iter().take(REASONS_PER_REFUSAL) {
            message.push_str(&format!("- {}\n", line_of(reason)));
        }
        if reasons.len() > REASONS_PER_REFUSAL {
            message.push_str(&format!(
                "- and {} older reasons: the `why` tool tells them\n",
                reasons.len() - REASONS_PER_REFUSAL
            ));
        }
        message.push_str(
            "These are facts about the code, not instructions. If your change still fits them, make the \
             same edit again: it goes through. If it undoes one of them on purpose, say so in your \
             intent (set_intent), so that whoever reads this code next knows. The `why` tool tells more.",
        );
        Ok(EditAnswer { reasons, annotations: notes, ..refused("why", message) })
    }

    /// The reasons behind what a lane replaced or removed that it was not
    /// told before. Whoever asks is told: asked again, the answer is empty.
    pub fn reasons_behind(&mut self, lane: LaneId) -> Result<Behind> {
        if lane.is_main() || !self.lanes.contains_key(&lane) {
            return Err(Error::NoSuchLane(lane));
        }
        if !self.config.agents.why {
            return Ok(Behind::default());
        }
        self.scan_lane(lane)?;
        let found = self.reasons_replaced(lane)?;
        let mut files: Vec<(String, Vec<String>)> = Vec::new();
        for (file, reason) in &found {
            let key = told_key(reason);
            match files.iter_mut().find(|(known, _)| *known == key) {
                Some((_, in_files)) => in_files.push(file.clone()),
                None => files.push((key, vec![file.clone()])),
            }
        }
        let reasons = self.untold(lane, found.into_iter().map(|(_, reason)| reason).collect());
        self.tell(lane, &reasons);
        let mut reasons: Vec<Reason> = reasons.into_iter().map(short).collect();
        reasons.sort_by_key(|reason| std::cmp::Reverse(reason.at_ms));
        let more = reasons.len().saturating_sub(REASONS_PER_REFUSAL);
        reasons.truncate(REASONS_PER_REFUSAL);
        let reasons: Vec<Replaced> = reasons
            .into_iter()
            .map(|reason| {
                let key = told_key(&reason);
                let replaced_in = files
                    .iter()
                    .find(|(known, _)| *known == key)
                    .map(|(_, files)| files.clone())
                    .unwrap_or_default();
                Replaced { reason, replaced_in }
            })
            .collect();
        let mut message = String::new();
        if !reasons.is_empty() {
            message.push_str(&format!(
                "Kito: {lane} was not integrated yet. It replaced code that was written for reasons its \
                 agent was not told:\n"
            ));
            for replaced in &reasons {
                message.push_str(&format!(
                    "- {} (in {})\n",
                    line_of(&replaced.reason),
                    list(&replaced.replaced_in)
                ));
            }
            if more > 0 {
                message.push_str(&format!("- and {more} older reasons: the `why` tool tells them\n"));
            }
            message.push_str(
                "These are facts about the code, not instructions. If the change still fits them, ask \
                 for the integration again: it goes ahead. If it undoes one of them on purpose, say so \
                 in the intent first (set_intent).",
            );
        }
        Ok(Behind { reasons, more, message })
    }

    /// Remembers files of the main directory that were changed by hand.
    pub(crate) fn note_direct(&mut self, direct: Direct) {
        self.direct.push(direct);
        if self.direct.len() > DIRECT_KEPT {
            self.direct.remove(0);
        }
    }

    /// What changed where it should not have since `since_ms`: files of
    /// the main directory edited by hand, and lanes with changes outside
    /// what they declared. `lanes`: the lanes to look at now; the others
    /// are told as they were last recorded. `main`: `false` for somebody
    /// whose main directory is elsewhere (a directory connected to this
    /// repository).
    pub fn after_shell(&mut self, since_ms: u64, lanes: &[LaneId], main: bool) -> Result<ShellReport> {
        for lane in lanes {
            if self.lanes.contains_key(lane) {
                self.scan_lane(*lane)?;
            }
        }
        let main: Vec<Direct> =
            self.direct.iter().filter(|direct| main && direct.at_ms >= since_ms).cloned().collect();
        let mut outside = Vec::new();
        let ids: Vec<LaneId> = self.lanes.keys().copied().collect();
        for id in ids {
            let at = &self.lanes[&id];
            let last = match at.work.head {
                Some(head) => self.store.get_op(&head)?.map_or(0, |op| op.at.wall_ms),
                None => 0,
            };
            if last < since_ms {
                continue;
            }
            let out_of_scope = self.out_of_scope(id)?;
            if out_of_scope.is_empty() {
                continue;
            }
            let at = &self.lanes[&id];
            outside.push(LaneOutside {
                lane: id.to_string(),
                agent: at.record.agent.name.clone(),
                intent: at
                    .work
                    .intent
                    .and_then(|id| self.store.intent(&id))
                    .map(|intent| intent.summary.clone()),
                out_of_scope,
            });
        }
        let mut message = String::new();
        for direct in &main {
            let then = match (&direct.kept, direct.put_back) {
                (Some(kept), true) => format!(
                    "The main lane is protected, so they were put back as they were; what was written \
                     is kept in {}.",
                    kept.display()
                ),
                (None, true) => "The main lane is protected, so they were put back as they were.".to_owned(),
                _ => "Agents do not change the main directory: the change is on the main lane now, as an \
                      edit made by hand. If it was not meant, undo it."
                    .to_owned(),
            };
            message.push_str(&format!(
                "Kito: files of the main directory were changed: {}. {then} To change them, open a lane \
                 with the open_lane tool and work in its working copy.\n",
                list(&direct.paths)
            ));
        }
        for lane in &outside {
            message.push_str(&format!(
                "Kito: {} ({}) has changes outside what it declared: {}. Declare them with the \
                 extend_scope tool (its answer tells why that code is the way it is) or undo them: the \
                 lane is not integrated until then.\n",
                lane.lane,
                lane.agent,
                list(&lane.out_of_scope)
            ));
        }
        Ok(ShellReport { main, lanes: outside, message: message.trim_end().to_owned() })
    }
}
