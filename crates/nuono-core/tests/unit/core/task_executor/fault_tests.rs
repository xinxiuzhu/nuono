use super::*;
use crate::error::NuonoError;
use serde_json::json;
use std::future::pending;
use tokio::time::timeout;

const TEST_DEADLINE: Duration = Duration::from_secs(3);

struct DropSpy(Option<oneshot::Sender<()>>);

impl Drop for DropSpy {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

fn drop_spy() -> (DropSpy, oneshot::Receiver<()>) {
    let (sender, receiver) = oneshot::channel();
    (DropSpy(Some(sender)), receiver)
}

fn stalled_task(spy: DropSpy, started: Option<oneshot::Sender<()>>) -> TaskFn {
    Box::new(move || {
        Box::pin(async move {
            let _spy = spy;
            if let Some(started) = started {
                let _ = started.send(());
            }
            pending::<Result<serde_json::Value>>().await
        })
    })
}

fn metadata(name: &str) -> TaskMetadata {
    TaskMetadata::new(name, "executor_fault_test", 0)
}

async fn wait_for_status(executor: &TaskExecutor, id: &str, status: TaskStatus) {
    timeout(TEST_DEADLINE, async {
        loop {
            if executor
                .get_task_status(id)
                .await
                .is_some_and(|metadata| metadata.status == status)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("task did not reach its expected state");
}

async fn wait_until_idle(executor: &TaskExecutor) {
    timeout(TEST_DEADLINE, async {
        loop {
            let stats = executor.stats().await;
            if stats.pending == 0 && stats.running == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled task retained executor capacity");
    assert!(executor.active_tasks().await.is_empty());
}

async fn assert_dropped(receiver: oneshot::Receiver<()>) {
    timeout(TEST_DEADLINE, receiver)
        .await
        .expect("task retained its captured resources")
        .expect("drop notification sender disappeared without running Drop");
}

#[tokio::test]
async fn capacity_includes_running_and_pending_and_rejects_without_waiting() {
    let executor = TaskExecutor::with_capacity(1, 2);
    let running = metadata("running");
    let running_id = running.task_id.clone();
    let (running_spy, running_dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    executor
        .submit(running, stalled_task(running_spy, Some(started_tx)))
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();

    let queued = metadata("queued");
    let queued_id = queued.task_id.clone();
    let (queued_spy, queued_dropped) = drop_spy();
    executor
        .submit(queued, stalled_task(queued_spy, None))
        .await
        .unwrap();
    wait_for_status(&executor, &queued_id, TaskStatus::Pending).await;
    let stats = executor.stats().await;
    assert_eq!((stats.running, stats.pending), (1, 1));
    assert_eq!(executor.active_tasks().await.len(), 2);

    let (rejected_spy, rejected_dropped) = drop_spy();
    let error = timeout(
        Duration::from_millis(250),
        executor.submit(metadata("overflow"), stalled_task(rejected_spy, None)),
    )
    .await
    .expect("submit waited for capacity instead of rejecting overload")
    .unwrap_err();
    assert!(matches!(error, NuonoError::Conflict(_)));
    assert_dropped(rejected_dropped).await;

    let (rejected_spy, rejected_dropped) = drop_spy();
    let error = timeout(
        Duration::from_millis(250),
        executor.submit_and_wait(metadata("wait_overflow"), stalled_task(rejected_spy, None)),
    )
    .await
    .expect("submit_and_wait waited for capacity instead of rejecting overload")
    .unwrap_err();
    assert!(matches!(error, NuonoError::Conflict(_)));
    assert_dropped(rejected_dropped).await;

    assert!(executor.cancel(&queued_id).await);
    assert_dropped(queued_dropped).await;
    assert!(executor.cancel(&running_id).await);
    assert_dropped(running_dropped).await;
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
    assert_eq!(executor.stats().await.cancelled, 2);
}

#[tokio::test]
async fn repeated_pending_cancellation_releases_captures_and_capacity() {
    let executor = TaskExecutor::with_capacity(1, 2);
    let blocker = metadata("blocker");
    let blocker_id = blocker.task_id.clone();
    let (spy, dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    executor
        .submit(blocker, stalled_task(spy, Some(started_tx)))
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();

    for _ in 0..256 {
        let task = metadata("cancel_before_start");
        let task_id = task.task_id.clone();
        let resource = Arc::new(vec![0_u8; 16 * 1024]);
        let weak = Arc::downgrade(&resource);
        let func: TaskFn = Box::new(move || {
            Box::pin(async move {
                let _resource = resource;
                pending::<Result<serde_json::Value>>().await
            })
        });
        executor.submit(task, func).await.unwrap();
        wait_for_status(&executor, &task_id, TaskStatus::Pending).await;
        assert_eq!(executor.stats().await.pending, 1);
        assert!(executor.cancel(&task_id).await);
        assert!(weak.upgrade().is_none(), "cancelled closure was retained");
        assert!(!executor.cancel(&task_id).await);
        let stats = executor.stats().await;
        assert_eq!((stats.running, stats.pending), (1, 0));
        let history = executor.task_history(Some(1)).await;
        assert_eq!(history[0].metadata.task_id, task_id);
        assert_eq!(history[0].metadata.status, TaskStatus::Cancelled);
        assert!(history[0].metadata.started_at.is_none());
        assert!(history[0].metadata.completed_at.is_some());
    }

    assert_eq!(executor.stats().await.cancelled, 256);
    assert!(executor.cancel(&blocker_id).await);
    assert_dropped(dropped).await;
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
    wait_until_idle(&executor).await;
}

#[tokio::test]
async fn dropping_a_pending_waiter_removes_its_closure_and_restores_capacity() {
    let executor = Arc::new(TaskExecutor::with_capacity(1, 2));
    let blocker = metadata("blocker");
    let blocker_id = blocker.task_id.clone();
    let (spy, dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    executor
        .submit(blocker, stalled_task(spy, Some(started_tx)))
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();

    let queued = metadata("cancelled_waiter");
    let queued_id = queued.task_id.clone();
    let (queued_spy, queued_dropped) = drop_spy();
    let waiter_executor = executor.clone();
    let waiter = tokio::spawn(async move {
        waiter_executor
            .submit_and_wait(queued, stalled_task(queued_spy, None))
            .await
    });
    wait_for_status(&executor, &queued_id, TaskStatus::Pending).await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_dropped(queued_dropped).await;
    let stats = executor.stats().await;
    assert_eq!((stats.running, stats.pending, stats.cancelled), (1, 0, 1));

    let replacement = metadata("replacement");
    let replacement_id = replacement.task_id.clone();
    let (replacement_spy, replacement_dropped) = drop_spy();
    executor
        .submit(replacement, stalled_task(replacement_spy, None))
        .await
        .unwrap();
    assert!(executor.cancel(&replacement_id).await);
    assert_dropped(replacement_dropped).await;
    assert!(executor.cancel(&blocker_id).await);
    assert_dropped(dropped).await;
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
}

#[tokio::test]
async fn dropping_a_running_waiter_drops_the_future_and_restores_capacity() {
    let executor = Arc::new(TaskExecutor::with_capacity(1, 1));
    let task = metadata("running_waiter");
    let (spy, dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    let waiter_executor = executor.clone();
    let waiter = tokio::spawn(async move {
        waiter_executor
            .submit_and_wait(task, stalled_task(spy, Some(started_tx)))
            .await
    });
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_dropped(dropped).await;
    wait_until_idle(&executor).await;
    assert_eq!(executor.stats().await.cancelled, 1);

    let result = timeout(
        TEST_DEADLINE,
        executor.submit_and_wait(
            metadata("after_cancellation"),
            Box::new(|| Box::pin(async { Ok(json!("capacity recovered")) })),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.metadata.status, TaskStatus::Completed);
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
}

#[tokio::test]
async fn explicit_running_cancellation_notifies_waiter_after_resource_cleanup() {
    let executor = Arc::new(TaskExecutor::with_capacity(1, 1));
    let task = metadata("explicit_cancel");
    let task_id = task.task_id.clone();
    let (spy, mut dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    let waiter_executor = executor.clone();
    let waiter = tokio::spawn(async move {
        waiter_executor
            .submit_and_wait(task, stalled_task(spy, Some(started_tx)))
            .await
    });
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();
    assert!(executor.cancel(&task_id).await);
    let result = timeout(TEST_DEADLINE, waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.metadata.status, TaskStatus::Cancelled);
    assert_eq!(dropped.try_recv(), Ok(()));
    wait_until_idle(&executor).await;
    assert!(!executor.cancel(&task_id).await);
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
}

#[tokio::test]
async fn duplicate_active_ids_cannot_replace_the_original_task() {
    let executor = TaskExecutor::with_capacity(1, 3);
    let original = metadata("original");
    let task_id = original.task_id.clone();
    let (spy, original_dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    executor
        .submit(original.clone(), stalled_task(spy, Some(started_tx)))
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();

    let (spy, dropped) = drop_spy();
    let error = executor
        .submit(original.clone(), stalled_task(spy, None))
        .await
        .unwrap_err();
    assert!(matches!(error, NuonoError::Conflict(_)));
    assert_dropped(dropped).await;

    let (spy, dropped) = drop_spy();
    let error = executor
        .submit_and_wait(original, stalled_task(spy, None))
        .await
        .unwrap_err();
    assert!(matches!(error, NuonoError::Conflict(_)));
    assert_dropped(dropped).await;
    assert_eq!(executor.active_tasks().await.len(), 1);
    assert_eq!(executor.stats().await.running, 1);
    assert!(executor.cancel(&task_id).await);
    assert_dropped(original_dropped).await;
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
    assert_eq!(executor.stats().await.cancelled, 1);
}

#[tokio::test]
async fn dropping_an_old_waiter_does_not_cancel_a_reused_task_id() {
    let executor = TaskExecutor::with_capacity(1, 1);
    let task = metadata("reused_id");
    let task_id = task.task_id.clone();
    let (started_tx, started_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    let mut old_waiter = Box::pin(executor.submit_and_wait(
        task.clone(),
        Box::new(move || {
            Box::pin(async move {
                let _ = started_tx.send(());
                let _ = finish_rx.await;
                Ok(json!("first generation completed"))
            })
        }),
    ));
    assert!(futures::poll!(&mut old_waiter).is_pending());
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();
    finish_tx.send(()).unwrap();
    // 在终态结果已发送后故意不再轮询旧的等待句柄，验证执行器不会泄漏或阻塞。
    wait_until_idle(&executor).await;
    assert_eq!(executor.stats().await.completed, 1);

    let (spy, mut dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    executor
        .submit(
            task,
            Box::new(move || {
                Box::pin(async move {
                    let _spy = spy;
                    let _ = started_tx.send(());
                    let _ = finish_rx.await;
                    Ok(json!("second generation completed"))
                })
            }),
        )
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();
    drop(old_waiter);
    tokio::task::yield_now().await;
    assert_eq!(dropped.try_recv(), Err(oneshot::error::TryRecvError::Empty));
    assert_eq!(
        executor.get_task_status(&task_id).await.unwrap().status,
        TaskStatus::Running
    );
    assert_eq!(executor.stats().await.cancelled, 0);

    finish_tx.send(()).unwrap();
    assert_dropped(dropped).await;
    wait_until_idle(&executor).await;
    assert_eq!(executor.stats().await.completed, 2);
    assert_eq!(executor.stats().await.cancelled, 0);
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
}

#[tokio::test]
async fn creation_panic_poll_panic_and_timeout_release_slots_and_resources() {
    let executor = TaskExecutor::with_capacity(1, 1);

    let (spy, dropped) = drop_spy();
    let factory_panic: TaskFn = Box::new(move || {
        let _spy = spy;
        panic!("injected panic while constructing the task future");
    });
    let result = timeout(
        TEST_DEADLINE,
        executor.submit_and_wait(metadata("factory_panic"), factory_panic),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.metadata.status, TaskStatus::Failed);
    assert_dropped(dropped).await;
    wait_until_idle(&executor).await;

    let (spy, dropped) = drop_spy();
    let polling_panic: TaskFn = Box::new(move || {
        Box::pin(async move {
            let _spy = spy;
            tokio::task::yield_now().await;
            panic!("injected panic while polling the task future");
        })
    });
    let result = timeout(
        TEST_DEADLINE,
        executor.submit_and_wait(metadata("polling_panic"), polling_panic),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.metadata.status, TaskStatus::Failed);
    assert_dropped(dropped).await;
    wait_until_idle(&executor).await;

    let (spy, dropped) = drop_spy();
    let result = timeout(
        TEST_DEADLINE,
        executor.submit_and_wait(
            metadata("timed_out").with_timeout(Duration::from_millis(20)),
            stalled_task(spy, None),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.metadata.status, TaskStatus::TimedOut);
    assert_dropped(dropped).await;
    wait_until_idle(&executor).await;
    let stats = executor.stats().await;
    assert_eq!((stats.failed, stats.timed_out), (2, 1));

    let result = executor
        .submit_and_wait(
            metadata("after_faults"),
            Box::new(|| Box::pin(async { Ok(json!(true)) })),
        )
        .await
        .unwrap();
    assert_eq!(result.metadata.status, TaskStatus::Completed);
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
}

#[tokio::test]
async fn dropping_executor_cancels_running_and_pending_tasks() {
    let executor = TaskExecutor::with_capacity(1, 2);
    let (running_spy, running_dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    executor
        .submit(
            metadata("never_finishes"),
            stalled_task(running_spy, Some(started_tx)),
        )
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();
    let (queued_spy, queued_dropped) = drop_spy();
    executor
        .submit(metadata("never_starts"), stalled_task(queued_spy, None))
        .await
        .unwrap();
    assert_eq!(executor.active_tasks().await.len(), 2);

    drop(executor);
    assert_dropped(running_dropped).await;
    assert_dropped(queued_dropped).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_pending_cancellation_to_finish_dropping_captures() {
    struct BlockingDrop {
        entered: Option<oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            // 即使断言失败导致通道关闭，也会一并释放该工作线程，防止测试卡死。
            let _ = self.release.recv_timeout(TEST_DEADLINE);
        }
    }

    let executor = Arc::new(TaskExecutor::with_capacity(1, 2));
    let (started_tx, started_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    let (running_spy, running_dropped) = drop_spy();
    executor
        .submit(
            metadata("running_until_pending_is_removed"),
            Box::new(move || {
                Box::pin(async move {
                    let _spy = running_spy;
                    let _ = started_tx.send(());
                    let _ = finish_rx.await;
                    Ok(json!(true))
                })
            }),
        )
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();

    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let capture = BlockingDrop {
        entered: Some(entered_tx),
        release: release_rx,
    };
    let task = metadata("cancel_while_capture_drop_is_blocked");
    let task_id = task.task_id.clone();
    executor
        .submit(
            task,
            Box::new(move || {
                Box::pin(async move {
                    let _capture = capture;
                    pending::<Result<serde_json::Value>>().await
                })
            }),
        )
        .await
        .unwrap();

    let cancellation_executor = executor.clone();
    let runtime = tokio::runtime::Handle::current();
    let cancellation = tokio::task::spawn_blocking(move || {
        runtime.block_on(cancellation_executor.cancel(&task_id))
    });
    timeout(TEST_DEADLINE, entered_rx).await.unwrap().unwrap();
    finish_tx.send(()).unwrap();
    assert_dropped(running_dropped).await;
    timeout(TEST_DEADLINE, async {
        while executor.stats().await.running != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(executor.stats().await.pending, 1);

    let mut shutdown = Box::pin(executor.shutdown());
    assert!(
        timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err(),
        "shutdown returned before cancelled captures finished dropping"
    );
    release_tx.send(()).unwrap();
    assert!(timeout(TEST_DEADLINE, cancellation).await.unwrap().unwrap());
    timeout(TEST_DEADLINE, shutdown).await.unwrap();
    let stats = executor.stats().await;
    assert_eq!((stats.completed, stats.cancelled, stats.pending), (1, 1, 0));
}

#[tokio::test]
async fn panicking_capture_destructors_do_not_leak_capacity_or_interrupt_bulk_cleanup() {
    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("injected panic while dropping a pending task capture");
        }
    }

    fn panicking_task(spy: DropSpy) -> TaskFn {
        let capture = (PanicOnDrop, spy);
        Box::new(move || {
            Box::pin(async move {
                let _capture = capture;
                pending::<Result<serde_json::Value>>().await
            })
        })
    }

    let executor = TaskExecutor::with_capacity(1, 2);
    let blocker = metadata("single_cancel_blocker");
    let blocker_id = blocker.task_id.clone();
    let (spy, blocker_dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    executor
        .submit(blocker, stalled_task(spy, Some(started_tx)))
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();

    let queued = metadata("panics_on_cancel");
    let queued_id = queued.task_id.clone();
    let (spy, dropped) = drop_spy();
    executor.submit(queued, panicking_task(spy)).await.unwrap();
    assert!(executor.cancel(&queued_id).await);
    assert_dropped(dropped).await;
    let stats = executor.stats().await;
    assert_eq!((stats.running, stats.pending, stats.cancelled), (1, 0, 1));

    let replacement = metadata("capacity_after_drop_panic");
    let replacement_id = replacement.task_id.clone();
    let (spy, dropped) = drop_spy();
    executor
        .submit(replacement, stalled_task(spy, None))
        .await
        .unwrap();
    assert!(executor.cancel(&replacement_id).await);
    assert_dropped(dropped).await;
    assert!(executor.cancel(&blocker_id).await);
    assert_dropped(blocker_dropped).await;
    timeout(TEST_DEADLINE, executor.shutdown()).await.unwrap();
    wait_until_idle(&executor).await;

    let executor = TaskExecutor::with_capacity(1, 4);
    let (spy, running_dropped) = drop_spy();
    let (started_tx, started_rx) = oneshot::channel();
    executor
        .submit(
            metadata("bulk_cancel_blocker"),
            stalled_task(spy, Some(started_tx)),
        )
        .await
        .unwrap();
    timeout(TEST_DEADLINE, started_rx).await.unwrap().unwrap();

    let mut panicking = metadata("bulk_panicking_capture");
    panicking.task_id = "000_panicking_capture".into();
    let (spy, panicking_dropped) = drop_spy();
    executor
        .submit(panicking, panicking_task(spy))
        .await
        .unwrap();
    let mut other_drops = Vec::new();
    for id in ["001_first_other_capture", "002_second_other_capture"] {
        let mut queued = metadata("bulk_normal_capture");
        queued.task_id = id.into();
        let (spy, dropped) = drop_spy();
        executor
            .submit(queued, stalled_task(spy, None))
            .await
            .unwrap();
        other_drops.push(dropped);
    }
    assert_eq!(executor.active_tasks().await.len(), 4);

    // 析构时会 panic 的闭包排在队列中其他待执行任务前面，验证清理流程不会漏掉后续任务。
    drop(executor);
    assert_dropped(panicking_dropped).await;
    for dropped in other_drops {
        assert_dropped(dropped).await;
    }
    assert_dropped(running_dropped).await;
}
