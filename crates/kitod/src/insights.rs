//! Search and numbers over what a repository did (ADR 0016): its reasons,
//! lanes, tasks, comments, notes and the code they changed; how lanes
//! flow, how each assistant and model does, where the code is busy, how
//! review goes.
//!
//! The moments that leave nothing in the history (an edit refused or held
//! back, checks that failed, conflicts) are kept as they happen, in
//! `.kito/activity.jsonl`: every event the daemon tells, but saves.

use crate::error::Result;
use crate::lanes::LaneInfo;
use crate::rpc::Event;
use crate::Repo;
use kito_core::LaneId;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const ACTIVITY: &str = "activity.jsonl";

/// Where the activity of the repository in `root` is kept.
pub fn activity_file(root: &Path) -> PathBuf {
    root.join(kito_store::STORE_DIR).join(ACTIVITY)
}

/// Keeps one event, unless it is one there are too many of to keep.
pub fn keep(file: &Path, event: &Event) {
    if matches!(event.kind.as_str(), "saved") {
        return;
    }
    use std::io::Write as _;
    if let Ok(line) = serde_json::to_string(event) {
        if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = writeln!(out, "{line}");
        }
    }
}

/// What was kept, oldest first.
pub fn activity(root: &Path) -> Vec<Event> {
    std::fs::read_to_string(activity_file(root))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// What one search asks.
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// Every word must be there, whatever its case.
    pub words: Vec<String>,
    /// `reason`, `lane`, `task`, `comment`, `note`, `change`; none: all.
    pub kinds: Vec<String>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    /// A person, an agent, an assistant or a model.
    pub who: Option<String>,
    /// A file, a directory (`src/`) or a function (`file:name`).
    pub place: Option<String>,
    /// `integrated`, `given_up`, `open`, `refused`, `conflicted`, `failed_checks`.
    pub outcome: Option<String>,
    pub limit: usize,
}

/// What happened to each lane, read from what was kept.
#[derive(Clone, Debug, Default)]
struct Story {
    opened_ms: Option<u64>,
    requested_ms: Option<u64>,
    integrated_ms: Option<u64>,
    closed_ms: Option<u64>,
    refused: usize,
    held: usize,
    conflicts: usize,
    failed_checks: usize,
    out_of_scope: usize,
    first_review_ms: Option<u64>,
}

fn stories(events: &[Event]) -> BTreeMap<String, Story> {
    let mut all: BTreeMap<String, Story> = BTreeMap::new();
    for event in events {
        let Some(lane) = event.lane.as_deref().filter(|lane| lane.starts_with("lane/")) else { continue };
        let story = all.entry(lane.to_owned()).or_default();
        match event.kind.as_str() {
            "lane_opened" => story.opened_ms = Some(event.at_ms),
            "integration_requested" => story.requested_ms = story.requested_ms.or(Some(event.at_ms)),
            "integrated" => story.integrated_ms = Some(event.at_ms),
            "lane_closed" => story.closed_ms = Some(event.at_ms),
            "edit_refused" => story.refused += 1,
            "edit_held" => story.held += 1,
            "conflicts" => story.conflicts += 1,
            "verification_failed" => story.failed_checks += 1,
            "out_of_scope" => story.out_of_scope += 1,
            "reviewed" => story.first_review_ms = story.first_review_ms.or(Some(event.at_ms)),
            _ => {}
        }
    }
    all
}

fn outcome_of(lane: &LaneInfo, story: Option<&Story>) -> &'static str {
    match (lane.state.as_str(), story) {
        (_, Some(story)) if story.integrated_ms.is_some() => "integrated",
        ("closed", _) => "given_up",
        _ => "open",
    }
}

fn lower(text: &str) -> String {
    text.to_lowercase()
}

/// `true` if every word is in the text.
fn has_all(words: &[String], text: &str) -> bool {
    let text = lower(text);
    words.iter().all(|word| text.contains(word.as_str()))
}

/// `true` if `place` is one of `paths`, a directory above one, or a
/// function of one.
fn is_at(place: &str, paths: &[String]) -> bool {
    let place = place.trim();
    paths.iter().any(|path| {
        path == place
            || (place.ends_with('/') && path.starts_with(place))
            || path.split(':').next() == Some(place)
            || place.split(':').next() == Some(path.as_str())
    })
}

fn median(mut values: Vec<u64>) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

const WEEK_MS: u64 = 7 * 24 * 3600 * 1000;

impl Repo {
    /// What matches a search, the best first, then the latest.
    pub fn search(&mut self, query: &Query) -> Result<Vec<Value>> {
        let words: Vec<String> =
            query.words.iter().map(|word| lower(word)).filter(|word| !word.is_empty()).collect();
        let wants = |kind: &str| query.kinds.is_empty() || query.kinds.iter().any(|k| k == kind);
        let events = activity(&self.root);
        let stories = stories(&events);
        let lanes = self.lanes(true);
        let in_time = |at: Option<u64>| match (at, query.since_ms, query.until_ms) {
            (None, None, None) => true,
            (None, _, _) => false,
            (Some(at), since, until) => {
                since.is_none_or(|since| at >= since) && until.is_none_or(|until| at <= until)
            }
        };
        let who_is = |names: &[&str]| match &query.who {
            None => true,
            Some(who) => names.iter().any(|name| lower(name).contains(&lower(who))),
        };
        let lane_of = |id: &str| lanes.iter().find(|lane| lane.id == id);
        let lane_outcome = |lane: &LaneInfo| -> Vec<&'static str> {
            let story = stories.get(&lane.id);
            let mut all = vec![outcome_of(lane, story)];
            if let Some(story) = story {
                if story.refused + story.held + story.out_of_scope > 0 {
                    all.push("refused");
                }
                if story.conflicts > 0 {
                    all.push("conflicted");
                }
                if story.failed_checks > 0 {
                    all.push("failed_checks");
                }
            }
            all
        };
        let outcome_ok = |lane: Option<&LaneInfo>| match (&query.outcome, lane) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(wanted), Some(lane)) => lane_outcome(lane).contains(&wanted.as_str()),
        };
        let assistant_of = |lane: &LaneInfo| -> (String, String) {
            lane.assistant
                .as_ref()
                .map(|a| (a.name.clone(), a.model.clone().unwrap_or_default()))
                .unwrap_or_default()
        };
        let mut found: Vec<(usize, u64, Value)> = Vec::new();
        let mut push = |score: usize, at: u64, hit: Value| found.push((score, at, hit));

        if wants("lane") || wants("change") {
            for lane in &lanes {
                let story = stories.get(&lane.id);
                let at = story.and_then(|story| story.opened_ms);
                let (assistant, model) = assistant_of(lane);
                let names = [lane.agent.as_str(), lane.owner.as_deref().unwrap_or(""), &assistant, &model];
                let mut places: Vec<String> = lane.files.clone();
                places.extend(lane.scope.iter().cloned());
                if !in_time(at) || !who_is(&names) || !outcome_ok(Some(lane)) {
                    continue;
                }
                if query.place.as_deref().is_some_and(|place| !is_at(place, &places)) {
                    continue;
                }
                let text = format!(
                    "{} {} {} {} {} {} {}",
                    lane.intent.as_deref().unwrap_or(""),
                    lane.task.as_deref().unwrap_or(""),
                    lane.agent,
                    lane.owner.as_deref().unwrap_or(""),
                    assistant,
                    model,
                    places.join(" ")
                );
                if !has_all(&words, &text) {
                    continue;
                }
                let outcome = lane_outcome(lane);
                if wants("lane") {
                    push(
                        words.len() + 1,
                        at.unwrap_or(0),
                        json!({
                            "kind": "lane", "id": lane.id, "title": lane.intent, "task": lane.task,
                            "by": lane.agent, "for": lane.owner, "assistant": lane.assistant,
                            "at_ms": at, "outcome": outcome, "where": places,
                        }),
                    );
                }
                if wants("change") && !lane.files.is_empty() {
                    let files: Vec<&String> = match &query.place {
                        Some(place) => {
                            lane.files.iter().filter(|file| is_at(place, &[(*file).clone()])).collect()
                        }
                        None => lane.files.iter().collect(),
                    };
                    push(
                        words.len(),
                        at.unwrap_or(0),
                        json!({
                            "kind": "change", "id": lane.id, "title": lane.intent, "by": lane.agent,
                            "at_ms": at, "where": files, "outcome": outcome,
                        }),
                    );
                }
            }
        }
        // What landed: each reason, and what it changed (a closed lane keeps no files of its own).
        if (wants("reason") || wants("change")) && !words.is_empty() {
            for hit in self.reasons(&words.join(" "), 200)? {
                let lane = hit.lane.as_deref().and_then(lane_of);
                let mut names: Vec<&str> = hit.by.iter().map(String::as_str).collect();
                if let Some(person) = hit.person.as_deref() {
                    names.push(person);
                }
                if !in_time(Some(hit.at_ms))
                    || !who_is(&names)
                    || (query.outcome.is_some() && !outcome_ok(lane))
                {
                    continue;
                }
                if query.place.as_deref().is_some_and(|place| !is_at(place, &hit.changed)) {
                    continue;
                }
                if wants("change") && !hit.changed.is_empty() {
                    let outcome = lane.map(&lane_outcome);
                    push(
                        words.len(),
                        hit.at_ms,
                        json!({ "kind": "change", "id": hit.checkpoint, "lane": hit.lane, "title": hit.why, "by": hit.by, "at_ms": hit.at_ms, "where": hit.changed, "outcome": outcome }),
                    );
                }
                if wants("reason") {
                    push(
                        words.len() + 2,
                        hit.at_ms,
                        json!({ "kind": "reason", "title": hit.why, "task": hit.task, "by": hit.by, "for": hit.person, "lane": hit.lane, "checkpoint": hit.checkpoint, "at_ms": hit.at_ms, "where": hit.changed }),
                    );
                }
            }
        }
        if wants("comment") && query.place.is_none() {
            for lane in &lanes {
                let id: LaneId = match lane.id.parse() {
                    Ok(id) => id,
                    Err(_) => continue,
                };
                if !outcome_ok(Some(lane)) {
                    continue;
                }
                for comment in self.comments(id, true).unwrap_or_default() {
                    if in_time(Some(comment.at_ms))
                        && who_is(&[comment.by.as_str()])
                        && has_all(&words, &comment.text)
                    {
                        push(
                            words.len(),
                            comment.at_ms,
                            json!({ "kind": "comment", "id": comment.id, "lane": lane.id, "title": comment.text, "by": comment.by, "at_ms": comment.at_ms, "on": comment.on }),
                        );
                    }
                }
            }
        }
        if (wants("task") || wants("comment")) && query.place.is_none() && query.outcome.is_none() {
            for task in crate::tasks::load(&self.root).unwrap_or_default().tasks {
                let id = crate::tasks::id(task.n);
                let names = [
                    task.by.as_str(),
                    task.assignee.as_deref().unwrap_or(""),
                    task.claimed_by.as_deref().unwrap_or(""),
                ];
                if wants("task")
                    && in_time(Some(task.at_ms))
                    && who_is(&names)
                    && has_all(&words, &format!("{id} {} {}", task.title, task.text))
                {
                    push(
                        words.len() + 1,
                        task.at_ms,
                        json!({ "kind": "task", "id": id, "title": task.title, "by": task.by, "for": task.assignee, "at_ms": task.at_ms, "outcome": task.state }),
                    );
                }
                if wants("comment") {
                    for comment in &task.comments {
                        if in_time(Some(comment.at_ms))
                            && who_is(&[comment.by.as_str()])
                            && has_all(&words, &comment.text)
                        {
                            push(
                                words.len(),
                                comment.at_ms,
                                json!({ "kind": "comment", "id": format!("{id}/{}", comment.n), "task": id, "title": comment.text, "by": comment.by, "at_ms": comment.at_ms }),
                            );
                        }
                    }
                }
            }
        }
        if wants("note") && query.outcome.is_none() {
            for note in self.notes(LaneId::MAIN, None).unwrap_or_default() {
                if !in_time(Some(note.at_ms))
                    || !who_is(&[note.by.as_str()])
                    || !has_all(&words, &format!("{} {}", note.on, note.text))
                {
                    continue;
                }
                if query.place.as_deref().is_some_and(|place| !is_at(place, std::slice::from_ref(&note.on))) {
                    continue;
                }
                push(
                    words.len(),
                    note.at_ms,
                    json!({ "kind": "note", "id": note.id, "title": note.text, "by": note.by, "at_ms": note.at_ms, "on": note.on }),
                );
            }
        }
        found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        Ok(found.into_iter().take(query.limit.max(1)).map(|(_, _, hit)| hit).collect())
    }

    /// The numbers of a repository between two times (none: all of it).
    pub fn stats(&self, since_ms: Option<u64>, until_ms: Option<u64>) -> Result<Value> {
        let in_time =
            |at: u64| since_ms.is_none_or(|since| at >= since) && until_ms.is_none_or(|until| at <= until);
        let events: Vec<Event> =
            activity(&self.root).into_iter().filter(|event| in_time(event.at_ms)).collect();
        let stories = stories(&events);
        let lanes = self.lanes(true);

        // Flow, week by week.
        let mut weeks: BTreeMap<u64, (u64, u64, u64)> = BTreeMap::new();
        for event in &events {
            let week = event.at_ms / WEEK_MS * WEEK_MS;
            let entry = weeks.entry(week).or_default();
            match event.kind.as_str() {
                "lane_opened" => entry.0 += 1,
                "integrated" => entry.1 += 1,
                _ => {}
            }
        }
        for (lane, story) in &stories {
            if let (Some(closed), None) = (story.closed_ms, story.integrated_ms) {
                let _ = lane;
                weeks.entry(closed / WEEK_MS * WEEK_MS).or_default().2 += 1;
            }
        }
        let lead: Vec<u64> =
            stories.values().filter_map(|s| Some(s.integrated_ms?.saturating_sub(s.opened_ms?))).collect();
        let waiting: Vec<u64> =
            stories.values().filter_map(|s| Some(s.integrated_ms?.saturating_sub(s.requested_ms?))).collect();
        let to_review: Vec<u64> = stories
            .values()
            .filter_map(|s| Some(s.first_review_ms?.saturating_sub(s.requested_ms?)))
            .collect();

        // Each assistant and model.
        let mut by_assistant: BTreeMap<(String, String), BTreeMap<&str, u64>> = BTreeMap::new();
        let mut hot_files: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut hot_functions: BTreeMap<String, u64> = BTreeMap::new();
        for lane in &lanes {
            let story = stories.get(&lane.id);
            let opened = story.and_then(|story| story.opened_ms);
            if (since_ms.is_some() || until_ms.is_some()) && opened.is_none() {
                continue;
            }
            let key = lane
                .assistant
                .as_ref()
                .map(|a| (a.name.clone(), a.model.clone().unwrap_or_default()))
                .unwrap_or_else(|| ("not said".to_owned(), String::new()));
            let row = by_assistant.entry(key).or_default();
            *row.entry("lanes").or_default() += 1;
            let story = story.cloned().unwrap_or_default();
            *row.entry("integrated").or_default() += u64::from(story.integrated_ms.is_some());
            *row.entry("given_up").or_default() +=
                u64::from(story.integrated_ms.is_none() && lane.state == "closed");
            *row.entry("conflicts").or_default() += story.conflicts as u64;
            *row.entry("refused").or_default() += (story.refused + story.out_of_scope) as u64;
            *row.entry("held").or_default() += story.held as u64;
            *row.entry("failed_checks").or_default() += story.failed_checks as u64;
            // What it changed, or, closed, what it declared: a closed lane keeps no files.
            let touched: BTreeSet<String> = lane
                .files
                .iter()
                .chain(lane.scope.iter())
                .map(|item| item.split(':').next().unwrap_or(item).to_owned())
                .filter(|path| !path.ends_with('/'))
                .collect();
            for file in &touched {
                let entry = hot_files.entry(file.clone()).or_default();
                entry.0 += 1;
                entry.1 += story.conflicts as u64;
            }
            for item in lane.scope.iter().filter(|item| item.contains(':')) {
                *hot_functions.entry(item.clone()).or_default() += 1;
            }
        }
        let assistants: Vec<Value> = by_assistant
            .into_iter()
            .map(|((name, model), counts)| {
                let lanes = counts.get("lanes").copied().unwrap_or(0);
                let integrated = counts.get("integrated").copied().unwrap_or(0);
                json!({
                    "assistant": name, "model": if model.is_empty() { Value::Null } else { json!(model) },
                    "lanes": lanes, "integrated": integrated,
                    "integration_rate": if lanes == 0 { 0.0 } else { integrated as f64 / lanes as f64 },
                    "given_up": counts.get("given_up"), "conflicts": counts.get("conflicts"),
                    "refused": counts.get("refused"), "held": counts.get("held"), "failed_checks": counts.get("failed_checks"),
                })
            })
            .collect();
        let mut files: Vec<(String, (u64, u64))> = hot_files.into_iter().collect();
        files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut functions: Vec<(String, u64)> = hot_functions.into_iter().collect();
        functions.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        // Review, by whoever reviews.
        let mut reviewers: BTreeMap<String, (u64, u64, Vec<u64>)> = BTreeMap::new();
        let requested: BTreeMap<&str, u64> =
            stories.iter().filter_map(|(lane, s)| Some((lane.as_str(), s.requested_ms?))).collect();
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        for event in events.iter().filter(|event| event.kind == "reviewed") {
            let who = event.by.clone().unwrap_or_else(|| "somebody".to_owned());
            let entry = reviewers.entry(who.clone()).or_default();
            if event.what.starts_with("approved") {
                entry.0 += 1;
            } else if event.what.starts_with("changes requested") {
                entry.1 += 1;
            }
            let lane = event.lane.clone().unwrap_or_default();
            if let Some(asked) = requested.get(lane.as_str()) {
                if seen.insert((who, lane)) {
                    entry.2.push(event.at_ms.saturating_sub(*asked));
                }
            }
        }
        let review: Vec<Value> = reviewers
            .into_iter()
            .map(|(who, (approved, changes, times))| json!({ "who": who, "approved": approved, "changes_requested": changes, "median_ms": median(times) }))
            .collect();

        Ok(json!({
            "since_ms": since_ms,
            "until_ms": until_ms,
            "kept_from_ms": activity(&self.root).first().map(|event| event.at_ms),
            "flow": {
                "weeks": weeks.into_iter().map(|(week, (opened, integrated, given_up))| json!({ "week_ms": week, "opened": opened, "integrated": integrated, "given_up": given_up })).collect::<Vec<_>>(),
                "open_now": lanes.iter().filter(|lane| lane.state != "closed").count(),
                "median_open_to_integrated_ms": median(lead),
                "median_waiting_ms": median(waiting),
                "median_to_first_review_ms": median(to_review),
            },
            "assistants": assistants,
            "hotspots": {
                "files": files.into_iter().take(15).map(|(path, (lanes, conflicts))| json!({ "path": path, "lanes": lanes, "conflicts": conflicts })).collect::<Vec<_>>(),
                "functions": functions.into_iter().take(15).map(|(name, lanes)| json!({ "name": name, "lanes": lanes })).collect::<Vec<_>>(),
            },
            "review": review,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, lane: &str, at_ms: u64) -> Event {
        Event {
            seq: 0,
            at_ms,
            kind: kind.into(),
            lane: Some(lane.into()),
            by: Some("ada".into()),
            what: String::new(),
            paths: Vec::new(),
        }
    }

    #[test]
    fn a_lane_s_story_is_read_from_what_was_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kito")).unwrap();
        let file = activity_file(dir.path());
        for kept in [
            event("lane_opened", "lane/1", 10),
            event("saved", "lane/1", 11),
            event("edit_refused", "lane/1", 12),
            event("integration_requested", "lane/1", 20),
            event("reviewed", "lane/1", 25),
            event("integrated", "lane/1", 30),
        ] {
            keep(&file, &kept);
        }
        let kept = activity(dir.path());
        assert_eq!(kept.len(), 5, "saves are not kept");
        let story = &stories(&kept)["lane/1"];
        assert_eq!(
            (story.opened_ms, story.integrated_ms, story.refused, story.first_review_ms),
            (Some(10), Some(30), 1, Some(25))
        );
        assert_eq!(median(vec![5, 1, 3]), Some(3));
        assert!(
            is_at("src/", &["src/shop.ts".into()]) && is_at("src/shop.ts", &["src/shop.ts:total".into()])
        );
        assert!(!is_at("lib/", &["src/shop.ts".into()]));
    }
}
