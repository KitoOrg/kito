//! The queue of the integrations: one at a time, in the order they were asked.

use crate::result::IntegrationResult;
use kito_core::LaneId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;
use tokio::sync::Notify;

/// Number of an integration request (`job#12`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "job#{}", self.0)
    }
}

impl FromStr for JobId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let number = s.trim().strip_prefix("job#").unwrap_or(s.trim());
        number.parse().map(JobId).map_err(|_| format!("{s:?} is not a job (job#12)"))
    }
}

/// Where a request is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    /// Waiting: `ahead` requests come before it.
    Queued {
        lane: String,
        ahead: usize,
    },
    /// Being merged or verified.
    Running {
        lane: String,
    },
    Done {
        result: Box<IntegrationResult>,
    },
}

/// How many finished requests are remembered.
const KEPT: usize = 256;

#[derive(Default)]
struct Inner {
    next: u64,
    waiting: VecDeque<(JobId, LaneId)>,
    /// The requests being served: one, or several checked together.
    running: Vec<(JobId, LaneId)>,
    done: BTreeMap<JobId, IntegrationResult>,
}

/// The integration queue. Requests come from any thread; one worker takes
/// them with [`Queue::next`] and gives the results back with [`Queue::finish`].
#[derive(Default)]
pub struct Queue {
    inner: Mutex<Inner>,
    /// A request arrived.
    arrived: Notify,
    /// A request finished.
    finished: Notify,
}

impl Queue {
    pub fn new() -> Queue {
        Queue::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Asks for the integration of a lane. A lane that is already waiting
    /// keeps its place: the same job is returned.
    pub fn submit(&self, lane: LaneId) -> JobId {
        let mut inner = self.lock();
        if let Some((job, _)) = inner.waiting.iter().find(|(_, l)| *l == lane) {
            return *job;
        }
        inner.next += 1;
        let job = JobId(inner.next);
        inner.waiting.push_back((job, lane));
        drop(inner);
        self.arrived.notify_one();
        job
    }

    /// Records a request that was served as soon as it was made: a lane
    /// landing on another lane, which waits for no checks and so for no
    /// lane landing on the main lane (ADR 0001).
    pub fn done_now(&self, result: IntegrationResult) -> JobId {
        let mut inner = self.lock();
        inner.next += 1;
        let job = JobId(inner.next);
        inner.done.insert(job, result);
        while inner.done.len() > KEPT {
            inner.done.pop_first();
        }
        drop(inner);
        self.finished.notify_waiters();
        job
    }

    /// The next request to serve. Waits until there is one.
    pub async fn next(&self) -> (JobId, LaneId) {
        self.next_batch(1, std::time::Duration::ZERO).await[0]
    }

    /// The next requests to serve together: every one that is waiting, in
    /// the order they were asked, up to `most`. Waits until there is one;
    /// then, when more than one may be taken, for `gather` more: requests
    /// made in the same breath (several lanes in one command) are served
    /// together.
    pub async fn next_batch(&self, most: usize, gather: std::time::Duration) -> Vec<(JobId, LaneId)> {
        let mut gathered = most <= 1 || gather.is_zero();
        loop {
            // Registered before looking: a request that arrives in between is not missed.
            let arrived = self.arrived.notified();
            let waiting = !self.lock().waiting.is_empty();
            if waiting && !gathered {
                tokio::time::sleep(gather).await;
                gathered = true;
                continue;
            }
            {
                let mut inner = self.lock();
                if !inner.waiting.is_empty() {
                    let take = inner.waiting.len().min(most.max(1));
                    let jobs: Vec<(JobId, LaneId)> = inner.waiting.drain(..take).collect();
                    inner.running.extend(jobs.iter().copied());
                    return jobs;
                }
            }
            arrived.await;
        }
    }

    /// Records the result of the request being served.
    pub fn finish(&self, job: JobId, result: IntegrationResult) {
        let mut inner = self.lock();
        inner.running.retain(|(running, _)| *running != job);
        inner.done.insert(job, result);
        while inner.done.len() > KEPT {
            inner.done.pop_first();
        }
        drop(inner);
        self.finished.notify_waiters();
    }

    /// Where a request is. `None`: unknown (never asked, or long forgotten).
    pub fn state(&self, job: JobId) -> Option<JobState> {
        let inner = self.lock();
        if let Some(result) = inner.done.get(&job) {
            return Some(JobState::Done { result: Box::new(result.clone()) });
        }
        if let Some((_, lane)) = inner.running.iter().find(|(running, _)| *running == job) {
            return Some(JobState::Running { lane: lane.to_string() });
        }
        let ahead = inner.waiting.iter().position(|(waiting, _)| *waiting == job)?;
        let ahead_with_running = ahead + inner.running.len();
        Some(JobState::Queued { lane: inner.waiting[ahead].1.to_string(), ahead: ahead_with_running })
    }

    /// The result of a request, waiting for it if it is not there yet.
    /// `None`: unknown request.
    pub async fn wait(&self, job: JobId) -> Option<IntegrationResult> {
        loop {
            let finished = self.finished.notified();
            match self.state(job)? {
                JobState::Done { result } => return Some(*result),
                _ => finished.await,
            }
        }
    }

    /// Requests waiting or being served.
    pub fn pending(&self) -> usize {
        let inner = self.lock();
        inner.waiting.len() + inner.running.len()
    }
}
