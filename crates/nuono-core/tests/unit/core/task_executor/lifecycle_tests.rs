use super::*;
use std::future::pending;
use tokio::sync::Barrier;
use tokio::time::timeout;

const TEST_DEADLINE: Duration = Duration::from_secs(3);

fn metadata(name: impl Into<String>) -> TaskMetadata {
    TaskMetadata::new(name, "executor_lifecycle_test", 0)
}

fn stalled_task() -> TaskFn {
    Box::new(|| Box::pin(pending::<Result<serde_json::Value>>()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_producers_cannot_exceed_total_admission_capacity() {
    const CAPACITY: usize = 4;
    const PRODUCERS: usize = 32;
    let executor = Arc::new(TaskExecutor::with_capacity(2, CAPACITY));
    let start = Arc::new(Barrier::new(PRODUCERS + 1));
    let mut producers = JoinSet::new();

    for index in 0..PRODUCERS {
        let executor = executor.clone();
        let start = start.clone();
        producers.spawn(async move {
            start.wait().await;
            timeout(
                TEST_DEADLINE,
                executor.submit(metadata(format!("producer_{index}")), stalled_task()),
            )
            .await
            .expect("submission waited for an unfinished task to release capacity")
        });
    }
    start.wait().await;

    let mut accepted = Vec::new();
    let mut rejected = 0;
    timeout(TEST_DEADLINE, async {
        while let Some(result) = producers.join_next().await {
            match result.expect("producer task panicked") {
                Ok(task_id) => accepted.push(task_id),
                Err(NuonoError::Conflict(_)) => rejected += 1,
                Err(error) => panic!("unexpected admission error: {error}"),
            }
        }
    })
    .await
    .expect("overloaded producers remained blocked");

    assert_eq!(accepted.len(), CAPACITY);
    assert_eq!(rejected, PRODUCERS - CAPACITY);
    let stats = executor.stats().await;
    assert_eq!(stats.pending + stats.running, CAPACITY);
    assert_eq!(executor.active_tasks().await.len(), CAPACITY);

    for task_id in accepted {
        assert!(executor.cancel(&task_id).await);
    }
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
    assert_eq!(executor.stats().await.cancelled, CAPACITY);
}

#[tokio::test]
async fn cancelling_shutdown_waiter_keeps_admission_closed_and_drains_existing_work() {
    let executor = TaskExecutor::with_capacity(1, 2);
    let (first_started_tx, first_started_rx) = oneshot::channel();
    let (release_first_tx, release_first_rx) = oneshot::channel();
    executor
        .submit(
            metadata("finishes_during_shutdown"),
            Box::new(move || {
                Box::pin(async move {
                    let _ = first_started_tx.send(());
                    release_first_rx.await.unwrap();
                    Ok(serde_json::Value::Null)
                })
            }),
        )
        .await
        .unwrap();
    timeout(TEST_DEADLINE, first_started_rx)
        .await
        .unwrap()
        .unwrap();

    let (second_started_tx, second_started_rx) = oneshot::channel();
    let second_id = executor
        .submit(
            metadata("starts_during_shutdown"),
            Box::new(move || {
                Box::pin(async move {
                    let _ = second_started_tx.send(());
                    pending::<Result<serde_json::Value>>().await
                })
            }),
        )
        .await
        .unwrap();

    let mut shutdown = Box::pin(executor.shutdown());
    assert!(futures::poll!(shutdown.as_mut()).is_pending());
    drop(shutdown);
    assert!(matches!(
        executor
            .submit(metadata("after_shutdown"), stalled_task())
            .await,
        Err(NuonoError::Conflict(_))
    ));

    release_first_tx.send(()).unwrap();
    timeout(TEST_DEADLINE, second_started_rx)
        .await
        .unwrap()
        .unwrap();
    let stats = executor.stats().await;
    assert_eq!((stats.completed, stats.running, stats.cancelled), (1, 1, 0));
    assert_eq!(
        executor.get_task_status(&second_id).await.unwrap().status,
        TaskStatus::Running
    );

    assert!(executor.cancel(&second_id).await);
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
    let stats = executor.stats().await;
    assert_eq!(
        (
            stats.completed,
            stats.cancelled,
            stats.pending,
            stats.running
        ),
        (1, 1, 0, 0)
    );
}

#[tokio::test]
async fn dropping_last_executor_owner_finishes_scheduler_and_releases_tracking() {
    let executor = TaskExecutor::with_capacity(1, 2);
    let (started_tx, started_rx) = oneshot::channel();
    executor
        .submit(
            metadata("running_at_owner_drop"),
            Box::new(move || {
                Box::pin(async move {
                    let _ = started_tx.send(());
                    pending::<Result<serde_json::Value>>().await
                })
            }),
        )
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();
    executor
        .submit(metadata("pending_at_owner_drop"), stalled_task())
        .await
        .unwrap();
    assert_eq!(executor.active_tasks().await.len(), 2);

    let tracking = Arc::downgrade(&executor.shared);
    let mut completed = executor.shutdown_complete_rx.clone();
    drop(executor);

    timeout(TEST_DEADLINE, async {
        while !*completed.borrow() {
            completed
                .changed()
                .await
                .expect("scheduler disappeared without publishing completion");
        }
        while tracking.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("scheduler or worker retained its shared tracking after owner drop");
    assert!(*completed.borrow());
    assert!(tracking.upgrade().is_none());
}
