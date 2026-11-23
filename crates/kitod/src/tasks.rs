//! Tasks (ADR 0015): what needs doing in a repository, and the lanes that
//! do it. Kept by Kito as metadata (`.kito/tasks.json`), never in the
//! project's files, and shared through the server as the rest is.
//!
//! A task is `task#<n>`. A lane belongs to one when it is opened with it as
//! its task; a task has as many lanes as it needs. It is done when the
//! person says so: once its last lane is closed it is ready to close, and
//! an agent may say it thinks it is done.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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
    /// What happened to it, in order (ADR 0024). Empty for a task from
    /// before Kito 0.41 that nothing happened to since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<TaskEvent>,
}

/// One thing that happened to a task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub at_ms: u64,
    pub by: String,
    /// `asked`, `edited`, `for`, `taken_up`, `comment`, `lane_opened`,
    /// `lane_integrated`, `lane_closed`, `thinks_done`, `done`, `reopened`,
    /// `dropped`.
    pub what: String,
    /// For `edited`: `title` or `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// For `for`: whom it is for now; `null`: nobody.
    #[serde(rename = "for", default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Option<String>>,
    /// For `taken_up` and `thinks_done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// For `comment`: which one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    /// For the lanes' events: `lane/3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// For `lane_integrated`: the checkpoint made, if one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// A field that is there, `null` included.
fn present<'de, D: serde::Deserializer<'de>>(given: D) -> Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::deserialize(given)?))
}

impl TaskEvent {
    pub fn new(what: &str, by: &str) -> TaskEvent {
        TaskEvent {
            at_ms: now_ms(),
            by: by.to_owned(),
            what: what.to_owned(),
            field: None,
            assignee: None,
            agent: None,
            n: None,
            lane: None,
            checkpoint: None,
        }
    }
}

impl Task {
    /// What happened to it: what was kept, or for a task from before Kito
    /// 0.41 what can be told without inventing: that it was asked for, and
    /// its comments.
    pub fn history(&self) -> Vec<TaskEvent> {
        if !self.history.is_empty() {
            return self.history.clone();
        }
        let asked = TaskEvent { at_ms: self.at_ms, ..TaskEvent::new("asked", &self.by) };
        let comments = self.comments.iter().map(|comment| TaskEvent {
            at_ms: comment.at_ms,
            n: Some(comment.n),
            ..TaskEvent::new("comment", &comment.by)
        });
        std::iter::once(asked).chain(comments).collect()
    }

    /// Notes something that happened to it. A task from before Kito 0.41
    /// was given what it can tell when it was taken to be changed
    /// ([`Tasks::get_mut`]): this comes after that.
    pub fn note(&mut self, event: TaskEvent) {
        self.seed();
        self.history.push(event);
    }

    /// For a task with no history: what it can tell, kept from now on,
    /// before anything of it changes.
    fn seed(&mut self) {
        if self.history.is_empty() {
            self.history = self.history();
        }
    }
}

/// A lane as a task needs it: nothing is worked out that a task does not
/// show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskLane {
    pub id: String,
    pub agent: String,
    /// As the lane was opened with it.
    pub task: Option<String>,
    pub state: String,
    pub owner: Option<String>,
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
    /// What happened to it, in order: in the answer of `task` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<TaskEvent>>,
}

/// Notes something that happened to the task a lane is for, if it is for
/// one of this repository's: `task` as the lane keeps it.
pub fn note_lane(root: &Path, lock: &Mutex<()>, task: Option<&str>, event: TaskEvent) {
    let Some(n) = task.and_then(lane_task) else { return };
    // What happened to a task is not worth failing what happened to its
    // lane, but it is said; a file that cannot be read is left as it is.
    let what = event.what.clone();
    let done = change(root, lock, |all| {
        if let Ok(found) = all.get_mut(n) {
            found.note(event);
        }
        Ok(())
    });
    if let Err(why) = done {
        eprintln!("kitod: {what} was not written in the history of {}: {why}", id(n));
    }
}

/// The task a lane is for, read from what the lane keeps exactly as it
/// always was (`task#3`, `#3`, `3`): the one rule that links a lane to a
/// task. A label written otherwise (`task #3`) is somebody's own.
pub fn lane_task(stored: &str) -> Option<u64> {
    let stored = stored.trim();
    let digits = stored.strip_prefix("task#").or_else(|| stored.strip_prefix('#')).unwrap_or(stored);
    digits.parse().ok().filter(|n| *n > 0)
}

/// Loads the tasks, changes them and saves them, with nothing else
/// changing them meanwhile. A file that cannot be read stops it, and is
/// left as it is.
pub fn change<T>(
    root: &Path,
    lock: &Mutex<()>,
    f: impl FnOnce(&mut Tasks) -> Result<T, String>,
) -> Result<T, String> {
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = load(root)?;
    let done = f(&mut all)?;
    save(root, &all)?;
    Ok(done)
}

pub fn id(n: u64) -> String {
    format!("task#{n}")
}

/// `task#3`, `task #3`, `Task #3`, `#3` or `3`: the number of a task, as
/// a request names one.
pub fn number(spec: &str) -> Option<u64> {
    let spec = spec.trim();
    named(spec).or_else(|| spec.strip_prefix('#').unwrap_or(spec).parse().ok().filter(|n| *n > 0))
}

/// The number of a task named with the word: `task#3`, `task #3`,
/// `Task #3`. What a lane is opened for may name a task only so.
pub fn named(spec: &str) -> Option<u64> {
    let spec = spec.trim();
    let word = spec.get(..4).filter(|word| word.eq_ignore_ascii_case("task"))?;
    let digits = spec[word.len()..].trim_start().strip_prefix('#')?;
    digits.parse().ok().filter(|n| *n > 0)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// The tasks: none when there is no file yet. A file that cannot be read
/// is an error, never taken for no tasks: what is saved next would be
/// written over every task it holds.
pub fn load(root: &Path) -> Result<Tasks, String> {
    let path = root.join(kito_store::STORE_DIR).join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Tasks::default()),
        Err(e) => return Err(unreadable(&path, &e.to_string())),
    };
    serde_json::from_str(&text).map_err(|e| unreadable(&path, &e.to_string()))
}

fn unreadable(path: &Path, why: &str) -> String {
    format!(
        "{} cannot be read ({why}): nothing is said or changed of the tasks until it is put right or moved \
         away",
        path.display()
    )
}

/// Saves the tasks as `kito_store::fs::atomic_write` writes a file:
/// whole beside it, on the disk, then renamed over it, and the rename on
/// the disk too. After a crash the file is the old one or the new one.
/// The staged file has a name of its own for each write.
pub fn save(root: &Path, tasks: &Tasks) -> Result<(), String> {
    use std::io::Write as _;
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let dir = root.join(kito_store::STORE_DIR);
    let path = dir.join(FILE);
    let text = serde_json::to_string_pretty(tasks).map_err(|e| e.to_string())?;
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let staged = dir.join(format!("{FILE}.{}-{n}{STAGED}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&staged, &path)?;
        kito_store::fs::sync_dir(&dir)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written.map_err(|e| format!("{}: {e}", path.display()))
}

/// The end of the name of a staged file of the tasks.
const STAGED: &str = ".new";

/// Removes the staged files of the tasks that other runs left behind (a
/// crash between the write and the rename): those of this process are
/// its writes going on.
pub fn remove_stale(root: &Path) {
    let dir = root.join(kito_store::STORE_DIR);
    let mine = format!("{FILE}.{}-", std::process::id());
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let staged = name.starts_with(&format!("{FILE}.")) && name.ends_with(STAGED);
        if staged && !name.starts_with(&mine) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

impl Tasks {
    pub fn get(&self, n: u64) -> Option<&Task> {
        self.tasks.iter().find(|task| task.n == n)
    }

    /// A task to change: one from before Kito 0.41 is first given the
    /// history it can tell, so that what changes now comes after it, once.
    pub fn get_mut(&mut self, n: u64) -> Result<&mut Task, String> {
        let task = self
            .tasks
            .iter_mut()
            .find(|task| task.n == n)
            .ok_or_else(|| format!("{}: no such task", id(n)))?;
        task.seed();
        Ok(task)
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
            history: vec![TaskEvent { at_ms: now, ..TaskEvent::new("asked", by) }],
        });
        Ok(self.next)
    }
}

/// The lanes of a task, among all the lanes there are.
fn lanes_of(n: u64, lanes: &[TaskLane]) -> impl Iterator<Item = &TaskLane> {
    lanes.iter().filter(move |lane| lane.task.as_deref().and_then(lane_task) == Some(n))
}

/// A task as the clients see it, with its lanes among `lanes` (all of
/// them, the closed ones too); with `whole`, its comments.
pub fn info(task: &Task, lanes: &[TaskLane], whole: bool) -> TaskInfo {
    let mine: Vec<&TaskLane> = lanes_of(task.n, lanes).collect();
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
        history: whole.then(|| task.history()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(id: &str, task: Option<&str>, state: &str) -> TaskLane {
        TaskLane {
            id: id.to_owned(),
            agent: "alice".to_owned(),
            task: task.map(str::to_owned),
            state: state.to_owned(),
            owner: None,
        }
    }

    #[test]
    fn a_task_is_open_then_in_a_lane_then_ready_then_done() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kito")).unwrap();
        let mut tasks = load(dir.path()).unwrap();
        assert!(tasks.add("x", "", "ada", None, None).is_err());
        let n = tasks.add("Prices in cents", "Everywhere.", "ada", Some("bob".into()), None).unwrap();
        assert_eq!((n, tasks.add("Another one", "", "ada", None, None).unwrap()), (1, 2));
        save(dir.path(), &tasks).unwrap();
        let tasks = load(dir.path()).unwrap();
        let task = tasks.get(1).unwrap();
        assert_eq!(info(task, &[], false).state, "open");
        let open = [lane("lane/1", Some("task#1"), "active"), lane("lane/2", Some("task#11"), "active")];
        let seen = info(task, &open, false);
        assert_eq!((seen.state, seen.lanes.len()), ("in_lane", 1));
        let closed = [lane("lane/1", Some("task#1"), "closed"), lane("lane/3", Some("prices"), "active")];
        assert_eq!(info(task, &closed, false).state, "ready");
        // A lane that keeps `task #1` was labelled so by somebody, as before
        // Kito 0.41: it is not task 1's.
        let labelled = [lane("lane/4", Some("task #1"), "active"), lane("lane/5", Some("Task #1"), "active")];
        assert_eq!(
            (info(task, &labelled, false).state, info(task, &labelled, false).lanes.len()),
            ("open", 0)
        );
        assert_eq!(
            [lane_task("task#1"), lane_task("#1"), lane_task("1"), lane_task("task #1")],
            [Some(1), Some(1), Some(1), None]
        );
        assert_eq!(
            [number("task#3"), number("#3"), number("3"), number("x"), number("task#0")],
            [Some(3), Some(3), Some(3), None, None]
        );
        // With the word, a space and any case: what people and agents write.
        assert_eq!(
            [number("task #55"), number("Task #55"), number("TASK#55"), number("task 55")],
            [Some(55), Some(55), Some(55), None]
        );
        assert_eq!(
            [named("task #3"), named("#3"), named("3"), named("tasks#3")],
            [Some(3), None, None, None]
        );
    }

    #[test]
    fn a_task_from_before_tells_what_it_can_and_keeps_the_rest_from_then_on() {
        // As 0.40 wrote it: no history.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kito")).unwrap();
        let old = serde_json::json!({ "next": 1, "tasks": [{
            "n": 1, "title": "Prices in cents", "text": "", "by": "ada", "state": "open",
            "at_ms": 1000, "updated_ms": 3000,
            "comments": [{ "n": 1, "by": "bob", "text": "which ones?", "at_ms": 2000 }]
        }]});
        std::fs::write(dir.path().join(".kito/tasks.json"), old.to_string()).unwrap();
        let mut tasks = load(dir.path()).unwrap();
        let said = |task: &Task| -> Vec<(u64, String, String)> {
            task.history().into_iter().map(|event| (event.at_ms, event.by, event.what)).collect()
        };
        let task = tasks.get(1).unwrap();
        assert_eq!(
            said(task),
            [(1000, "ada".into(), "asked".into()), (2000, "bob".into(), "comment".into())]
        );
        assert!(info(task, &[], false).history.is_none());
        // Something happens: what was told is kept, and this after it.
        tasks.get_mut(1).unwrap().note(TaskEvent { assignee: Some(None), ..TaskEvent::new("for", "ada") });
        save(dir.path(), &tasks).unwrap();
        let task = load(dir.path()).unwrap().get(1).unwrap().clone();
        assert_eq!(task.history.len(), 3);
        let last = serde_json::to_value(&task.history[2]).unwrap();
        assert_eq!((last["what"].as_str(), last.get("for")), (Some("for"), Some(&serde_json::Value::Null)));
    }

    #[test]
    fn a_comment_on_a_task_from_before_is_in_its_history_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kito")).unwrap();
        let old = serde_json::json!({ "next": 1, "tasks": [{
            "n": 1, "title": "Prices in cents", "text": "", "by": "ada", "state": "open",
            "at_ms": 1000, "updated_ms": 3000,
            "comments": [
                { "n": 1, "by": "bob", "text": "which ones?", "at_ms": 2000 },
                { "n": 2, "by": "ada", "text": "all of them", "at_ms": 2500 }
            ]
        }]});
        std::fs::write(dir.path().join(".kito/tasks.json"), old.to_string()).unwrap();
        let lock = Mutex::new(());
        // As the daemon comments: the comment first, then what happened.
        change(dir.path(), &lock, |all| {
            let task = all.get_mut(1)?;
            task.comments.push(TaskComment {
                n: 3,
                by: "bob".into(),
                text: "done".into(),
                at_ms: 4000,
                source: None,
            });
            task.note(TaskEvent { n: Some(3), ..TaskEvent::new("comment", "bob") });
            Ok(())
        })
        .unwrap();
        let task = load(dir.path()).unwrap().get(1).unwrap().clone();
        let said: Vec<(&str, Option<u64>)> =
            task.history.iter().map(|event| (event.what.as_str(), event.n)).collect();
        assert_eq!(said, [("asked", None), ("comment", Some(1)), ("comment", Some(2)), ("comment", Some(3))]);
    }

    #[test]
    fn tasks_changed_at_once_are_all_kept_and_an_unreadable_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kito")).unwrap();
        let lock = Mutex::new(());
        change(dir.path(), &lock, |all| all.add("Prices in cents", "", "ada", None, None)).unwrap();
        // Comments and lanes' events, from many threads at once.
        std::thread::scope(|scope| {
            for k in 0..16 {
                let (root, lock) = (dir.path(), &lock);
                scope.spawn(move || {
                    for i in 0..5 {
                        match k % 2 {
                            0 => change(root, lock, |all| {
                                let task = all.get_mut(1)?;
                                let n = task.comments.len() as u64 + 1;
                                task.comments.push(TaskComment {
                                    n,
                                    by: format!("agent {k}"),
                                    text: format!("comment {i}"),
                                    at_ms: now_ms(),
                                    source: None,
                                });
                                task.note(TaskEvent { n: Some(n), ..TaskEvent::new("comment", "bob") });
                                Ok(())
                            })
                            .unwrap(),
                            _ => note_lane(root, lock, Some("task#1"), TaskEvent::new("lane_opened", "bob")),
                        }
                    }
                });
            }
        });
        let task = load(dir.path()).unwrap().get(1).unwrap().clone();
        assert_eq!((task.comments.len(), task.history.len()), (40, 81));
        let staged = std::fs::read_dir(dir.path().join(".kito")).unwrap().count();
        assert_eq!(staged, 1, "only tasks.json is left");

        // What a crashed run left is removed; a write of this one is not.
        let kito = dir.path().join(".kito");
        std::fs::write(kito.join("tasks.json.1-7.new"), "half").unwrap();
        let ours = kito.join(format!("tasks.json.{}-99.new", std::process::id()));
        std::fs::write(&ours, "going on").unwrap();
        remove_stale(dir.path());
        assert!(!kito.join("tasks.json.1-7.new").exists() && ours.exists());
        std::fs::remove_file(&ours).unwrap();

        // A file that cannot be read refuses every change, and stays as it is.
        let path = dir.path().join(".kito/tasks.json");
        std::fs::write(&path, "{ not json").unwrap();
        let refused = change(dir.path(), &lock, |all| all.add("Another one", "", "ada", None, None));
        assert!(refused.unwrap_err().contains("cannot be read"));
        assert!(load(dir.path()).is_err());
        note_lane(dir.path(), &lock, Some("task#1"), TaskEvent::new("lane_closed", "bob"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
