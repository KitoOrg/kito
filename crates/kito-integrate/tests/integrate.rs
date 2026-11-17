//! The verifier and the queue.

use kito_core::LaneId;
use kito_integrate::{verify, Confinement, IntegrationResult, JobId, JobState, Outcome, Queue};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn commands(list: &[&str]) -> Vec<String> {
    list.iter().map(|c| (*c).to_owned()).collect()
}

#[tokio::test]
async fn commands_run_in_order_in_the_directory_and_stop_at_the_first_failure() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("marker.txt"), "here\n").unwrap();

    let passed = verify(
        tmp.path(),
        &commands(&[
            "test -f marker.txt",
            "test \"$KITO_VERIFY\" = 1",
            "echo one > trace",
            "echo two >> trace",
        ]),
        Duration::from_secs(30),
        &Confinement::default(),
    )
    .await;
    assert!(passed.passed, "{passed:?}");
    assert_eq!(passed.commands.len(), 4);
    assert_eq!((passed.failed_command.clone(), passed.output.clone()), (None, None));
    assert_eq!(std::fs::read_to_string(tmp.path().join("trace")).unwrap(), "one\ntwo\n");

    let failed = verify(
        tmp.path(),
        &commands(&["true", "echo checking; echo broken >&2; exit 3", "echo never > after"]),
        Duration::from_secs(30),
        &Confinement::default(),
    )
    .await;
    assert!(!failed.passed);
    assert_eq!(
        failed.commands,
        ["true", "echo checking; echo broken >&2; exit 3"],
        "what follows did not run"
    );
    assert_eq!(failed.failed_command.as_deref(), Some("echo checking; echo broken >&2; exit 3"));
    let output = failed.output.unwrap();
    assert!(
        output.contains("checking") && output.contains("broken") && output.contains("exit code 3"),
        "{output}"
    );
    assert!(!tmp.path().join("after").exists());

    // No commands: nothing to fail.
    assert!(verify(tmp.path(), &[], Duration::from_secs(1), &Confinement::default()).await.passed);
    // A command that cannot even start in a directory that is not there.
    let nowhere = verify(
        &tmp.path().join("missing"),
        &commands(&["true"]),
        Duration::from_secs(1),
        &Confinement::default(),
    )
    .await;
    assert!(!nowhere.passed && nowhere.output.unwrap().contains("could not be started"));
}

#[tokio::test]
async fn a_command_that_hangs_is_stopped_with_everything_it_started() {
    let tmp = tempfile::tempdir().unwrap();
    let started = Instant::now();
    // The shell starts a child that would write a file long after the timeout.
    let report = verify(
        tmp.path(),
        &commands(&["(sleep 3; echo late > late.txt) & sleep 60"]),
        Duration::from_millis(300),
        &Confinement::default(),
    )
    .await;
    assert!(!report.passed);
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(report.output.unwrap().contains("still running"));
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(!tmp.path().join("late.txt").exists(), "the child of the shell was killed too");
}

#[tokio::test]
async fn requests_are_served_one_at_a_time_in_order() {
    let queue = Arc::new(Queue::new());
    let (a, b, c) = (queue.submit(LaneId(1)), queue.submit(LaneId(2)), queue.submit(LaneId(3)));
    assert_eq!((a, b, c), (JobId(1), JobId(2), JobId(3)));
    // A lane that is already waiting keeps its place.
    assert_eq!(queue.submit(LaneId(2)), b);
    assert_eq!(queue.pending(), 3);
    assert_eq!(queue.state(c), Some(JobState::Queued { lane: "lane/3".into(), ahead: 2 }));
    assert_eq!(queue.state(JobId(99)), None);

    assert_eq!(queue.next().await, (a, LaneId(1)));
    assert_eq!(queue.state(a), Some(JobState::Running { lane: "lane/1".into() }));
    assert_eq!(queue.state(b), Some(JobState::Queued { lane: "lane/2".into(), ahead: 1 }));
    // While it is being served, the same lane can ask again: a new request.
    let again = queue.submit(LaneId(1));
    assert_eq!(again, JobId(4));

    // Someone waits for the result while the worker is still at it.
    let waiter = tokio::spawn({
        let queue = queue.clone();
        async move { queue.wait(a).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiter.is_finished());
    queue.finish(a, IntegrationResult::new("lane/1", Outcome::Integrated));
    let result = waiter.await.unwrap().unwrap();
    assert_eq!(result.result, Outcome::Integrated);
    assert!(matches!(queue.state(a), Some(JobState::Done { .. })));

    assert_eq!(queue.next().await, (b, LaneId(2)));
    queue.finish(b, IntegrationResult::failed("lane/2", "the lane was closed"));
    assert_eq!(queue.wait(b).await.unwrap().message.as_deref(), Some("the lane was closed"));
    assert_eq!(queue.next().await, (c, LaneId(3)));
    assert_eq!(queue.next().await, (again, LaneId(1)));
    assert_eq!(queue.wait(JobId(99)).await, None);

    // A worker that is waiting wakes up when a request arrives.
    queue.finish(c, IntegrationResult::new("lane/3", Outcome::NothingToIntegrate));
    queue.finish(again, IntegrationResult::new("lane/1", Outcome::NothingToIntegrate));
    let worker = tokio::spawn({
        let queue = queue.clone();
        async move { queue.next().await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let late = queue.submit(LaneId(7));
    assert_eq!(worker.await.unwrap(), (late, LaneId(7)));
}

#[test]
fn a_result_reads_like_the_one_in_the_plan() {
    let mut result = IntegrationResult::new("lane/b2", Outcome::Integrated);
    result.checkpoint = Some("ckpt#119".into());
    result.your_next_base = Some("ckpt#119".into());
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["lane"], "lane/b2");
    assert_eq!(json["result"], "integrated");
    assert_eq!(json["checkpoint"], "ckpt#119");
    assert_eq!(json["conflicts"], serde_json::json!([]));
    assert_eq!(json["auto_resolutions"], serde_json::json!([]));
    assert_eq!(json["your_next_base"], "ckpt#119");
    assert_eq!(serde_json::from_value::<IntegrationResult>(json).unwrap(), result);
}

#[tokio::test]
async fn requests_that_wait_are_served_together() {
    let queue = Arc::new(Queue::new());
    let lanes: Vec<LaneId> = (1..=4).map(LaneId).collect();
    let jobs: Vec<JobId> = lanes.iter().map(|lane| queue.submit(*lane)).collect();
    // Up to three at a time, in the order they were asked.
    let batch = queue.next_batch(3, Duration::ZERO).await;
    assert_eq!(batch, [(jobs[0], lanes[0]), (jobs[1], lanes[1]), (jobs[2], lanes[2])]);
    for job in &jobs[..3] {
        assert!(matches!(queue.state(*job), Some(JobState::Running { .. })));
    }
    // The one that waits has three ahead of it.
    assert!(matches!(queue.state(jobs[3]), Some(JobState::Queued { ahead: 3, .. })));
    queue.finish(jobs[1], IntegrationResult::new("lane/2", Outcome::Integrated));
    assert!(matches!(queue.state(jobs[1]), Some(JobState::Done { .. })));
    assert!(matches!(queue.state(jobs[3]), Some(JobState::Queued { ahead: 2, .. })));

    // A request made a moment after the first is taken with it.
    let queue = Arc::new(Queue::new());
    let first = queue.submit(LaneId(1));
    let late = queue.clone();
    let second = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        late.submit(LaneId(2))
    });
    let batch = queue.next_batch(8, Duration::from_millis(60)).await;
    assert_eq!(batch, [(first, LaneId(1)), (second.await.unwrap(), LaneId(2))]);
}
