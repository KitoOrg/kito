//! Tasks (ADR 0015): what needs doing in a repository, and the lanes that
//! do it. Kept by Kito as metadata (`.kito/tasks.json`), never in the
//! project's files, and shared through the server as the rest is.
//!
//! A task is `task#<n>`. A lane belongs to one when it is opened with it as
//! its task; a task has as many lanes as it needs. It is done when the
//! person says so: once its last lane is closed it is ready to close, and
//! an agent may say it thinks it is done.

use crate::lanes::LaneInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

const FILE: &str = "tasks.json";

/// Everything kept: the tasks, and the number the next one gets.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Tasks {
    pub next: u64,
    pub tasks: Vec<Task>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub n: u64,
    pub title: String,
    #[serde(default)]
    pub text: String,
    /// Who asked for it.
    pub by: String,
    /// Who it is for: a person or an agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// `open`, `done` or `dropped`.
    pub state: String,
    /// The agent that took it up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// The agent that thinks it is done, until somebody says it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinks_done: Option<String>,
    pub at_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub comments: Vec<TaskComment>,
    /// Where it comes from, when it was not asked for here: `{"kind":
    /// "github", "url": ..., "number": 12}`, as the plugin said it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskComment {
    pub n: u64,
    pub by: String,
    pub text: String,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
}

/// A task as the clients see it.
#[derive(Clone, Debug, Serialize)]
pub struct TaskInfo {
    /// `task#3`.
    pub id: String,
    pub title: String,
    pub text: String,
    pub by: String,
    #[serde(rename = "for", skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// `open`, `in_lane` (a lane is on it), `ready` (its lanes are all
    /// closed: the person may close it), `done` or `dropped`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinks_done: Option<String>,
    pub at_ms: u64,
    pub updated_ms: u64,
    /// Its lanes: id, agent, state.
    pub lanes: Vec<Value>,
    pub comment_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comments: Option<Vec<TaskComment>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
}

pub fn id(n: u64) -> String {
    format!("task#{n}")
}

/// `task#3`, `#3` or `3`: the number of a task.
pub fn number(spec: &str) -> Option<u64> {
    let spec = spec.trim();
    let digits = spec.strip_prefix("task#").or_else(|| spec.strip_prefix('#')).unwrap_or(spec);
    digits.parse().ok().filter(|n| *n > 0)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

pub fn load(root: &Path) -> Tasks {
    let path = root.join(kito_store::STORE_DIR).join(FILE);
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

pub fn save(root: &Path, tasks: &Tasks) -> Result<(), String> {
    let path = root.join(kito_store::STORE_DIR).join(FILE);
    let text = serde_json::to_string_pretty(tasks).map_err(|e| e.to_string())?;
    let staged = path.with_extension("json.new");
    std::fs::write(&staged, text)
        .and_then(|()| std::fs::rename(&staged, &path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

impl Tasks {
    pub fn get(&self, n: u64) -> Option<&Task> {
        self.tasks.iter().find(|task| task.n == n)
    }

    pub fn get_mut(&mut self, n: u64) -> Result<&mut Task, String> {
        self.tasks.iter_mut().find(|task| task.n == n).ok_or_else(|| format!("{}: no such task", id(n)))
    }

    /// Adds a task and answers its number.
    pub fn add(
        &mut self,
        title: &str,
        text: &str,
        by: &str,
        assignee: Option<String>,
        source: Option<Value>,
    ) -> Result<u64, String> {
        let title = title.trim();
        if title.chars().filter(|c| c.is_alphabetic()).count() < 2 {
            return Err("a task needs a title of a few words".into());
        }
        self.next = self.next.max(self.tasks.iter().map(|task| task.n).max().unwrap_or(0)) + 1;
        let now = now_ms();
        self.tasks.push(Task {
            n: self.next,
            title: title.to_owned(),
            text: text.trim().to_owned(),
            by: by.to_owned(),
            assignee: assignee.filter(|who| !who.trim().is_empty()),
            state: "open".into(),
            claimed_by: None,
            thinks_done: None,
            at_ms: now,
            updated_ms: now,
            comments: Vec::new(),
            source,
        });
        Ok(self.next)
    }
}

/// The lanes of a task, among all the lanes there are.
fn lanes_of(n: u64, lanes: &[LaneInfo]) -> impl Iterator<Item = &LaneInfo> {
    lanes.iter().filter(move |lane| lane.task.as_deref().and_then(number) == Some(n))
}

/// A task as the clients see it, with its lanes among `lanes` (all of
/// them, the closed ones too); with `whole`, its comments.
pub fn info(task: &Task, lanes: &[LaneInfo], whole: bool) -> TaskInfo {
    let mine: Vec<&LaneInfo> = lanes_of(task.n, lanes).collect();
    let open = mine.iter().any(|lane| lane.state != "closed");
    let state = match task.state.as_str() {
        "done" => "done",
        "dropped" => "dropped",
        _ if open => "in_lane",
        _ if !mine.is_empty() => "ready",
        _ => "open",
    };
    TaskInfo {
        id: id(task.n),
        title: task.title.clone(),
        text: task.text.clone(),
        by: task.by.clone(),
        assignee: task.assignee.clone(),
        state,
        claimed_by: task.claimed_by.clone(),
        thinks_done: task.thinks_done.clone(),
        at_ms: task.at_ms,
        updated_ms: task.updated_ms,
        lanes: mine
            .iter()
            .map(|lane| serde_json::json!({ "id": lane.id, "agent": lane.agent, "state": lane.state, "owner": lane.owner }))
            .collect(),
        comment_count: task.comments.len(),
        comments: whole.then(|| task.comments.clone()),
        source: task.source.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(id: &str, task: Option<&str>, state: &str) -> LaneInfo {
        serde_json::from_value(serde_json::json!({
            "id": id, "agent": "alice", "task": task, "intent": null, "base": "ckpt#1",
            "state": state, "workdir": null, "ops": 0, "files": [], "conflicts": []
        }))
        .unwrap()
    }

    #[test]
    fn a_task_is_open_then_in_a_lane_then_ready_then_done() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kito")).unwrap();
        let mut tasks = load(dir.path());
        assert!(tasks.add("x", "", "ada", None, None).is_err());
        let n = tasks.add("Prices in cents", "Everywhere.", "ada", Some("bob".into()), None).unwrap();
        assert_eq!((n, tasks.add("Another one", "", "ada", None, None).unwrap()), (1, 2));
        save(dir.path(), &tasks).unwrap();
        let tasks = load(dir.path());
        let task = tasks.get(1).unwrap();
        assert_eq!(info(task, &[], false).state, "open");
        let open = [lane("lane/1", Some("task#1"), "active"), lane("lane/2", Some("task#11"), "active")];
        let seen = info(task, &open, false);
        assert_eq!((seen.state, seen.lanes.len()), ("in_lane", 1));
        let closed = [lane("lane/1", Some("task#1"), "closed"), lane("lane/3", Some("prices"), "active")];
        assert_eq!(info(task, &closed, false).state, "ready");
        assert_eq!(
            [number("task#3"), number("#3"), number("3"), number("x"), number("task#0")],
            [Some(3), Some(3), Some(3), None, None]
        );
    }
}
