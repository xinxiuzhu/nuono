use super::*;
use std::sync::{
    Condvar,
    atomic::{AtomicBool, Ordering},
};

const TEST_TIMEOUT: Duration = Duration::from_secs(3);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("asset upload fault test timed out")
}

struct Gate(Arc<(Mutex<bool>, Condvar)>);
impl Gate {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }
    fn wait(&self) -> impl FnOnce() + Send + 'static {
        let state = self.0.clone();
        move || {
            let (lock, ready) = &*state;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = ready.wait(open).unwrap();
            }
        }
    }
    fn release(&self) {
        let (lock, ready) = &*self.0;
        *lock.lock().unwrap() = true;
        ready.notify_all();
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        self.release();
    }
}

struct DropProbe {
    uploads: Arc<AssetUploads>,
    dropped: Arc<AtomicBool>,
}
impl Drop for DropProbe {
    fn drop(&mut self) {
        assert_eq!(
            self.uploads.available_slots(),
            0,
            "payload outlived upload admission"
        );
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn default_two_slots_cover_all_shared_lease_clones() {
    let uploads = AssetUploads::new();
    let first = uploads.admit().unwrap();
    let clone = first.clone();
    let second = uploads.admit().unwrap();
    assert!(matches!(uploads.admit(), Err(NuonoError::Overloaded(_))));
    drop(first);
    assert!(matches!(uploads.admit(), Err(NuonoError::Overloaded(_))));
    drop(clone);
    assert_eq!(uploads.available_slots(), 1);
    let replacement = uploads.admit().unwrap();
    drop((replacement, second));
    bounded(uploads.wait()).await;
    assert_eq!(uploads.available_slots(), 2);
}

#[tokio::test]
async fn zero_capacity_rejects_without_registering_shutdown_activity() {
    let uploads = AssetUploads::with_capacity(0);
    assert!(matches!(uploads.admit(), Err(NuonoError::Overloaded(_))));
    bounded(uploads.wait()).await;
    assert!(matches!(uploads.admit(), Err(NuonoError::Conflict(_))));
}

#[tokio::test(start_paused = true)]
async fn stalled_body_read_expires_thirty_seconds_after_admission() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let started = tokio::time::Instant::now();
    assert!(matches!(
        lease.read(std::future::pending::<()>()).await,
        Err(NuonoError::Timeout(_))
    ));
    assert_eq!(started.elapsed(), BODY_TIMEOUT);
    assert_eq!(uploads.available_slots(), 0);
    drop(lease);
    assert_eq!(uploads.available_slots(), 1);
}

#[tokio::test(start_paused = true)]
async fn body_budget_is_absolute_and_does_not_reset_between_reads() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let started = tokio::time::Instant::now();
    lease
        .read(tokio::time::sleep(Duration::from_secs(25)))
        .await
        .unwrap();
    assert!(matches!(
        lease.read(std::future::pending::<()>()).await,
        Err(NuonoError::Timeout(_))
    ));
    assert_eq!(started.elapsed(), BODY_TIMEOUT);
}

#[tokio::test]
async fn close_cancels_an_admitted_reader_before_wait_can_finish() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let (started, ready) = oneshot::channel();
    let reader = tokio::spawn(async move {
        let _ = started.send(());
        lease.read(std::future::pending::<()>()).await
    });
    bounded(ready).await.unwrap();
    uploads.close();
    assert!(matches!(
        bounded(reader).await.unwrap(),
        Err(NuonoError::Conflict(_))
    ));
    bounded(uploads.wait()).await;
    assert_eq!(uploads.available_slots(), 1);
}

#[tokio::test]
async fn close_wins_against_a_body_future_that_is_already_ready() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    uploads.close();
    assert!(matches!(
        lease.read(std::future::ready(7)).await,
        Err(NuonoError::Conflict(_))
    ));
    drop(lease);
    bounded(uploads.wait()).await;
}

#[tokio::test]
async fn dropping_a_reader_releases_its_admission_without_spawning_a_pipeline() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let reader = tokio::spawn(async move { lease.read(std::future::pending::<()>()).await });
    tokio::task::yield_now().await;
    reader.abort();
    assert!(reader.await.unwrap_err().is_cancelled());
    assert_eq!(uploads.available_slots(), 1);
    assert_eq!(uploads.active_jobs(), 0);
}

#[tokio::test]
async fn cancelling_a_running_blocking_waiter_keeps_captures_and_slot_until_real_completion() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let gate = Gate::new();
    let wait = gate.wait();
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe {
        uploads: uploads.clone(),
        dropped: dropped.clone(),
    };
    let (started, ready) = oneshot::channel();
    let request = tokio::spawn(async move {
        lease
            .blocking(move || {
                let _probe = probe;
                let _ = started.send(());
                wait();
                3
            })
            .await
    });
    bounded(ready).await.unwrap();
    request.abort();
    assert!(matches!(request.await, Err(error) if error.is_cancelled()));
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(matches!(uploads.admit(), Err(NuonoError::Overloaded(_))));
    uploads.close();
    let waiting = tokio::spawn({
        let uploads = uploads.clone();
        async move { uploads.wait().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    gate.release();
    bounded(waiting).await.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(uploads.available_slots(), 1);
}

#[test]
fn cancelling_queued_blocking_work_retains_its_result_until_cleanup_before_releasing_slot() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let occupied = Gate::new();
    let wait = occupied.wait();
    let (started, ready) = oneshot::channel();
    let blocker = runtime.spawn_blocking(move || {
        let _ = started.send(());
        wait();
    });
    runtime.block_on(async {
        bounded(ready).await.unwrap();
        let uploads = AssetUploads::with_capacity(1);
        let lease = uploads.admit().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let probe = DropProbe {
            uploads: uploads.clone(),
            dropped: dropped.clone(),
        };
        let (submitted, accepted) = oneshot::channel();
        let request = tokio::spawn(async move {
            let _ = submitted.send(());
            lease.blocking(move || probe).await
        });
        bounded(accepted).await.unwrap();
        request.abort();
        assert!(matches!(request.await, Err(error) if error.is_cancelled()));
        assert!(!dropped.load(Ordering::SeqCst));
        assert_eq!(uploads.available_slots(), 0);
        uploads.close();
        let waiting = tokio::spawn({
            let uploads = uploads.clone();
            async move { uploads.wait().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        occupied.release();
        bounded(blocker).await.unwrap();
        bounded(waiting).await.unwrap();
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(uploads.available_slots(), 1);
    });
}

#[tokio::test]
async fn decoded_result_keeps_its_slot_while_publication_waits() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let publish = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe {
        uploads: uploads.clone(),
        dropped: dropped.clone(),
    };
    let (decoded, ready) = oneshot::channel();
    let request = tokio::spawn({
        let uploads = uploads.clone();
        let publish = publish.clone();
        async move {
            uploads
                .run(lease, move |lease| async move {
                    let result = lease.blocking(move || probe).await.unwrap();
                    let _ = decoded.send(());
                    publish.notified().await;
                    drop(result);
                    9
                })
                .await
        }
    });
    bounded(ready).await.unwrap();
    assert_eq!(uploads.active_jobs(), 1);
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(matches!(uploads.admit(), Err(NuonoError::Overloaded(_))));
    uploads.close();
    let waiting = tokio::spawn({
        let uploads = uploads.clone();
        async move { uploads.wait().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    publish.notify_one();
    assert_eq!(bounded(request).await.unwrap().unwrap(), 9);
    bounded(waiting).await.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(uploads.available_slots(), 1);
}

#[tokio::test]
async fn cancelling_http_response_does_not_cancel_an_accepted_publication() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let finish = Arc::new(tokio::sync::Notify::new());
    let published = Arc::new(AtomicBool::new(false));
    let (started, ready) = oneshot::channel();
    let request = tokio::spawn({
        let uploads = uploads.clone();
        let finish = finish.clone();
        let published = published.clone();
        async move {
            uploads
                .run(lease, move |_lease| async move {
                    let _ = started.send(());
                    finish.notified().await;
                    published.store(true, Ordering::SeqCst);
                })
                .await
        }
    });
    bounded(ready).await.unwrap();
    request.abort();
    assert!(matches!(request.await, Err(error) if error.is_cancelled()));
    assert_eq!(uploads.active_jobs(), 1);
    assert_eq!(uploads.available_slots(), 0);
    uploads.close();
    let waiting = tokio::spawn({
        let uploads = uploads.clone();
        async move { uploads.wait().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    finish.notify_one();
    bounded(waiting).await.unwrap();
    assert!(published.load(Ordering::SeqCst));
    assert_eq!(uploads.available_slots(), 1);
}

#[tokio::test(start_paused = true)]
async fn accepted_pipeline_outlives_body_deadline_and_close_to_finish_publication() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let (started, ready) = oneshot::channel();
    let request = tokio::spawn({
        let uploads = uploads.clone();
        async move {
            uploads
                .run(lease, move |_lease| async move {
                    let _ = started.send(());
                    tokio::time::sleep(BODY_TIMEOUT * 2).await;
                    42
                })
                .await
        }
    });
    ready.await.unwrap();
    uploads.close();
    assert_eq!(request.await.unwrap().unwrap(), 42);
    uploads.wait().await;
    assert_eq!(uploads.available_slots(), 1);
}

#[tokio::test]
async fn close_rejects_a_not_yet_polled_run_and_destroys_captures_before_releasing_lease() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe {
        uploads: uploads.clone(),
        dropped: dropped.clone(),
    };
    let operation = move |_lease| {
        let _probe = probe;
        std::future::ready(7)
    };
    let request = uploads.run(lease, operation);
    uploads.close();
    assert!(matches!(request.await, Err(NuonoError::Conflict(_))));
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(uploads.active_jobs(), 0);
    bounded(uploads.wait()).await;
    assert_eq!(uploads.available_slots(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_and_run_race_never_accepts_untracked_publication() {
    for _ in 0..16 {
        let uploads = AssetUploads::with_capacity(1);
        let lease = uploads.admit().unwrap();
        let ready = Arc::new(tokio::sync::Barrier::new(3));
        let published = Arc::new(AtomicBool::new(false));
        let request = tokio::spawn({
            let uploads = uploads.clone();
            let ready = ready.clone();
            let published = published.clone();
            async move {
                ready.wait().await;
                uploads
                    .run(lease, move |_lease| async move {
                        published.store(true, Ordering::SeqCst);
                        1
                    })
                    .await
            }
        });
        let close = tokio::spawn({
            let uploads = uploads.clone();
            let ready = ready.clone();
            async move {
                ready.wait().await;
                uploads.close();
            }
        });
        ready.wait().await;
        bounded(close).await.unwrap();
        let accepted = match bounded(request).await.unwrap() {
            Ok(1) => true,
            Err(NuonoError::Conflict(_)) => false,
            other => panic!("unexpected close/run result: {other:?}"),
        };
        bounded(uploads.wait()).await;
        assert_eq!(published.load(Ordering::SeqCst), accepted);
        assert_eq!(uploads.active_jobs(), 0);
        assert_eq!(uploads.available_slots(), 1);
    }
}

#[tokio::test]
async fn cancelling_shutdown_waiter_does_not_lose_the_shared_reader_barrier() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let first = tokio::spawn({
        let uploads = uploads.clone();
        async move { uploads.wait().await }
    });
    tokio::task::yield_now().await;
    assert!(!first.is_finished());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let second = tokio::spawn({
        let uploads = uploads.clone();
        async move { uploads.wait().await }
    });
    tokio::task::yield_now().await;
    assert!(!second.is_finished());
    drop(lease);
    bounded(second).await.unwrap();
    bounded(uploads.wait()).await;
}

#[tokio::test]
async fn a_foreign_lease_cannot_bypass_another_pools_capacity() {
    let first = AssetUploads::with_capacity(1);
    let second = AssetUploads::with_capacity(0);
    let lease = first.admit().unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let result = second
        .run(lease, {
            let called = called.clone();
            move |_lease| async move {
                called.store(true, Ordering::SeqCst);
            }
        })
        .await;
    assert!(matches!(result, Err(NuonoError::Conflict(_))));
    assert!(!called.load(Ordering::SeqCst));
    assert_eq!(first.available_slots(), 1);
    bounded(first.wait()).await;
    bounded(second.wait()).await;
}

#[tokio::test]
async fn pipeline_constructor_and_async_panic_release_captures_and_admission() {
    for constructor in [true, false] {
        let uploads = AssetUploads::with_capacity(1);
        let lease = uploads.admit().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let probe = DropProbe {
            uploads: uploads.clone(),
            dropped: dropped.clone(),
        };
        let result: Result<()> = uploads
            .run(lease, move |_lease| {
                if constructor {
                    let _probe = probe;
                    panic!("injected upload constructor panic");
                }
                async move {
                    let _probe = probe;
                    tokio::task::yield_now().await;
                    panic!("injected upload pipeline panic");
                }
            })
            .await;
        assert!(matches!(result, Err(NuonoError::Internal(_))));
        assert!(dropped.load(Ordering::SeqCst));
        bounded(uploads.wait()).await;
        assert_eq!(uploads.available_slots(), 1);
    }
}

#[tokio::test]
async fn blocking_panic_releases_captures_without_invalidating_an_outer_lease() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe {
        uploads: uploads.clone(),
        dropped: dropped.clone(),
    };
    let result: Result<()> = lease
        .blocking(move || {
            let _probe = probe;
            panic!("injected decoder panic");
        })
        .await;
    assert!(matches!(result, Err(NuonoError::Internal(_))));
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(uploads.available_slots(), 0);
    drop(lease);
    bounded(uploads.wait()).await;
    assert_eq!(uploads.available_slots(), 1);
}

#[tokio::test]
async fn completed_pipeline_result_still_owns_admission_until_its_waiter_reads_or_drops_it() {
    let uploads = AssetUploads::with_capacity(1);
    let lease = uploads.admit().unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe {
        uploads: uploads.clone(),
        dropped: dropped.clone(),
    };
    let mut request = Box::pin(uploads.run(lease, move |_lease| async move { probe }));
    assert!(futures::poll!(request.as_mut()).is_pending());
    bounded(async {
        while uploads.active_jobs() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // 后台任务已执行完毕并将结果放入 oneshot 通道，但调用方尚未 poll 读取该结果；
    // 此时并发槽位租约与停机等待屏障仍由该未消费结果持有。
    assert_eq!(uploads.available_slots(), 0);
    assert!(!dropped.load(Ordering::SeqCst));
    uploads.close();
    let waiting = tokio::spawn({
        let uploads = uploads.clone();
        async move { uploads.wait().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    drop(request);
    bounded(waiting).await.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(uploads.available_slots(), 1);
}
