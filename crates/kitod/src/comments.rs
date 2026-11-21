//! Comments on a lane (ADR 0004): what whoever reviews a lane says about
//! it, or about code in it, and what the agent in the lane is told before
//! it goes on.
//!
//! A comment on lines keeps what the lines said. While the lane still has
//! those lines, wherever they moved, the comment is on them; once it does
//! not, the comment is `outdated` and shown with what it quoted.

use crate::error::{Error, Result};
use crate::why::{parse_target, Asked};
use crate::Repo;
use kito_core::{Author, Comment, CommentId, CommentTarget, LaneId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// A comment as the clients see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentInfo {
    /// `comment#3`.
    pub id: String,
    pub lane: String,
    /// What it is on, as it is called now: `src/shop.ts:total`,
    /// `src/shop.ts:12-14`, `src/shop.ts`. None: the lane as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// The lines it is on, where they are now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<(u32, u32)>,
    /// What those lines said when it was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted: Option<String>,
    pub text: String,
    pub by: String,
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    pub at_ms: u64,
    /// The comment it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    /// The code it is on changed since, or is gone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outdated: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Where `quoted` is in `text` now, as lines from 1: the place nearest to
/// where it was.
fn find_quoted(text: &str, quoted: &str, was: u32) -> Option<(u32, u32)> {
    let (lines, wanted): (Vec<&str>, Vec<&str>) = (text.lines().collect(), quoted.lines().collect());
    if wanted.is_empty() || wanted.len() > lines.len() {
        return None;
    }
    (0..=lines.len() - wanted.len())
        .filter(|&at| lines[at..at + wanted.len()] == wanted[..])
        .min_by_key(|&at| (at as i64 + 1 - i64::from(was)).abs())
        .map(|at| (at as u32 + 1, (at + wanted.len()) as u32))
}

impl Repo {
    /// What a comment on `target` is on, as `lane` has the code: a
    /// function (`file:name`), a file, or lines.
    fn comment_target(&self, lane: LaneId, target: &str) -> Result<CommentTarget> {
        let text = kito_core::paths::portable_text(target);
        let text = text.trim();
        let work = self.work(lane)?;
        let no_file = |file: &Path| Error::Other(format!("{}: no such file on {lane}", file.display()));
        match parse_target(text)? {
            Asked::Symbol(file, name) => match work.symbols.find(&file, &name).as_slice() {
                [id] => {
                    Ok(CommentTarget { path: file, symbol: Some(*id), lines: None, quoted: String::new() })
                }
                [] if work.tree.contains(Path::new(text)) => {
                    Ok(CommentTarget { path: text.into(), symbol: None, lines: None, quoted: String::new() })
                }
                [] => Err(Error::Other(format!("{}: nothing called `{name}`", file.display()))),
                several => Err(Error::Other(format!(
                    "{} things are called `{name}` in {}: comment on one by its lines ({}:<line>)",
                    several.len(),
                    file.display(),
                    file.display()
                ))),
            },
            Asked::File(path) => {
                let path = crate::scope::clean(&path.to_string_lossy())?;
                match work.tree.contains(&path) {
                    true => Ok(CommentTarget { path, symbol: None, lines: None, quoted: String::new() }),
                    false => Err(no_file(&path)),
                }
            }
            Asked::Lines(file, from, to) => {
                if !work.tree.contains(&file) {
                    return Err(no_file(&file));
                }
                let content = self.text_of(lane, &file)?;
                let all: Vec<&str> = content.lines().collect();
                if from as usize > all.len() {
                    return Err(Error::Other(format!(
                        "{} has {} lines on {lane}: there is no line {from}",
                        file.display(),
                        all.len()
                    )));
                }
                let to = to.min(all.len() as u32);
                let spans = self.symbol_spans(lane, &file)?;
                let symbol =
                    spans.iter().rfind(|(_, start, end)| *start <= from && from <= *end).map(|span| span.0);
                let quoted = all[from as usize - 1..to as usize].join("\n");
                Ok(CommentTarget { path: file, symbol, lines: Some((from, to)), quoted })
            }
        }
    }

    /// Says something about a lane, about code in it (`target`), or in
    /// answer to a comment (`reply_to`). `by`: who says it (default:
    /// whoever runs the repository); `person`: whom an agent works for.
    pub fn add_comment(
        &mut self,
        lane: LaneId,
        target: Option<&str>,
        text: &str,
        by: Option<Author>,
        person: Option<Author>,
        reply_to: Option<CommentId>,
    ) -> Result<CommentInfo> {
        if lane.is_main() {
            return Err(Error::Other(
                "a comment is on a lane: what there is to know about code of the main lane is a note (`kito note`)"
                    .into(),
            ));
        }
        let text = text.trim();
        if text.chars().filter(|c| c.is_alphabetic()).count() < 2 {
            return Err(Error::Other("a comment needs a few words".into()));
        }
        // A comment is on the code as its agent just left it.
        self.scan_lane(lane)?;
        let reply_to = match reply_to {
            None => None,
            Some(id) => {
                let parent =
                    self.store.comment(id).ok_or_else(|| Error::Other(format!("{id}: no such comment")))?;
                if parent.lane != lane {
                    return Err(Error::Other(format!("{id} is on {}, not on {lane}", parent.lane)));
                }
                // An answer to an answer is an answer to what started it.
                let first = parent.reply_to.unwrap_or(parent.id);
                if self.store.comment(first).is_some_and(|first| first.resolved.is_some()) {
                    return Err(Error::Other(format!("{first} is resolved: say it in a new comment")));
                }
                Some(first)
            }
        };
        let on = match (target, reply_to) {
            (Some(target), None) => Some(self.comment_target(lane, target)?),
            (Some(_), Some(first)) => {
                return Err(Error::Other(format!("an answer is on what {first} is on: leave the target out")))
            }
            (None, _) => None,
        };
        let comment = Comment {
            id: self.store.next_comment_id(),
            lane,
            on,
            text: text.to_owned(),
            by: by.unwrap_or_else(|| self.identity.author()),
            person,
            at_ms: now_ms(),
            at_ops: self.lane_ops(lane)?,
            reply_to,
            resolved: None,
        };
        self.store.put_comment(&comment)?;
        Ok(self.comment_info(&comment))
    }

    /// The comments on a lane, each followed by its answers; with `all`,
    /// the resolved ones too.
    pub fn comments(&self, lane: LaneId, all: bool) -> Result<Vec<CommentInfo>> {
        if !lane.is_main() && self.store.lane_record(lane).is_none() {
            return Err(Error::NoSuchLane(lane));
        }
        let mut answers: BTreeMap<CommentId, Vec<&Comment>> = BTreeMap::new();
        let mut first = Vec::new();
        for comment in self.store.comments().filter(|comment| comment.lane == lane) {
            match comment.reply_to {
                Some(to) => answers.entry(to).or_default().push(comment),
                None => first.push(comment),
            }
        }
        let mut told = Vec::new();
        for comment in first.into_iter().filter(|comment| all || comment.resolved.is_none()) {
            told.push(self.comment_info(comment));
            told.extend(
                answers.get(&comment.id).into_iter().flatten().map(|answer| self.comment_info(answer)),
            );
        }
        Ok(told)
    }

    /// Resolves a comment: it is kept, and no longer told. For who wrote
    /// it, the owner of its lane, or somebody who may integrate; `person`:
    /// who asks, when a server established it.
    pub fn resolve_comment(
        &mut self,
        id: CommentId,
        by: Option<Author>,
        person: Option<&str>,
        integrator: bool,
    ) -> Result<CommentInfo> {
        let mut comment =
            self.store.comment(id).cloned().ok_or_else(|| Error::Other(format!("{id}: no such comment")))?;
        if let Some(first) = comment.reply_to {
            return Err(Error::Other(format!("{id} is an answer to {first}: resolve {first}")));
        }
        if comment.resolved.is_some() {
            return Err(Error::Other(format!("{id} is resolved already")));
        }
        if let (Some(person), false) = (person, integrator) {
            let wrote = comment.person.as_ref().unwrap_or(&comment.by).name == person;
            let owner = self.store.lane_record(comment.lane).and_then(|lane| lane.owner.as_ref());
            if !wrote && owner.is_none_or(|owner| owner.name != person) {
                return Err(Error::Refused(format!(
                    "{id} is resolved by who wrote it, by the owner of {}, or by somebody who may integrate",
                    comment.lane
                )));
            }
        }
        comment.resolved = Some((by.unwrap_or_else(|| self.identity.author()), now_ms()));
        self.store.put_comment(&comment)?;
        Ok(self.comment_info(&comment))
    }

    /// The comments of a lane its agent has to know: not resolved, and
    /// not its own.
    pub(crate) fn comments_for_agent(&self, lane: LaneId) -> Vec<CommentInfo> {
        let agent = self.store.lane_record(lane).map(|lane| lane.agent.name.clone());
        let all = self.comments(lane, false).unwrap_or_default();
        all.into_iter().filter(|comment| Some(&comment.by) != agent.as_ref()).collect()
    }

    fn comment_info(&self, comment: &Comment) -> CommentInfo {
        let work = self.work(comment.lane).ok();
        let (mut on, mut lines, mut outdated) = (None, None, false);
        if let Some(target) = &comment.on {
            // The function, where it is and as it is called now.
            let symbol = target.symbol.and_then(|id| {
                let work = work?;
                let entry = work.symbols.get(id)?;
                let name = work.symbols.qualified_name(id).unwrap_or_else(|| entry.name.clone());
                Some((entry.file.clone(), name))
            });
            let file = symbol.as_ref().map_or(&target.path, |(file, _)| file);
            let shown = kito_core::paths::portable_text(&file.display().to_string());
            match target.lines {
                Some((was, _)) => {
                    let found = work
                        .filter(|work| work.tree.contains(file))
                        .and_then(|_| self.text_of(comment.lane, file).ok())
                        .and_then(|text| find_quoted(&text, &target.quoted, was));
                    match found {
                        Some((from, to)) if from == to => on = Some(format!("{shown}:{from}")),
                        Some((from, to)) => on = Some(format!("{shown}:{from}-{to}")),
                        None => on = Some(shown),
                    }
                    lines = found;
                    // A lane that is closed is as the comment left it.
                    outdated = found.is_none() && work.is_some();
                }
                None => match (&symbol, target.symbol) {
                    (Some((_, name)), Some(id)) => {
                        on = Some(format!("{shown}:{name}"));
                        outdated = self.touched_since(comment.lane, id, comment.at_ops);
                    }
                    _ => {
                        on = Some(shown);
                        outdated = work.is_some_and(|work| !work.tree.contains(file));
                    }
                },
            }
        }
        CommentInfo {
            id: comment.id.to_string(),
            lane: comment.lane.to_string(),
            on,
            lines,
            quoted: comment.on.as_ref().filter(|on| on.lines.is_some()).map(|on| on.quoted.clone()),
            text: comment.text.clone(),
            by: comment.by.name.clone(),
            person: comment.person.as_ref().map(|person| person.name.clone()),
            at_ms: comment.at_ms,
            reply_to: comment.reply_to.map(|id| id.to_string()),
            resolved_by: comment.resolved.as_ref().map(|(by, _)| by.name.clone()),
            outdated,
        }
    }

    /// `true` if the lane recorded something on a symbol after its first
    /// `ops` ops.
    fn touched_since(&self, lane: LaneId, id: kito_core::SymbolId, ops: u64) -> bool {
        const PAGE: usize = 1024;
        let mut start = ops;
        loop {
            let Ok(page) = self.store.lane_ops(lane, start, PAGE) else { return false };
            if page.iter().any(|op| crate::review::symbol_of(&op.kind) == Some(id)) {
                return true;
            }
            if page.len() < PAGE {
                return false;
            }
            start += page.len() as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_quoted;

    #[test]
    fn quoted_lines_are_found_where_they_moved() {
        let text = "a\nb\nc\nd\nb\nc\n";
        assert_eq!(find_quoted(text, "b\nc", 2), Some((2, 3)));
        assert_eq!(find_quoted(text, "b\nc", 6), Some((5, 6)), "the place nearest to where it was");
        assert_eq!(find_quoted(text, "d", 1), Some((4, 4)));
        assert_eq!(find_quoted(text, "b\nd", 2), None);
        assert_eq!(find_quoted("", "b", 1), None);
    }
}
