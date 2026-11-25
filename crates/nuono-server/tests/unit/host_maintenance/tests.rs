use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot};

#[tokio::test(start_paused = true)]
async fn cancellation_during_startup_delay_never_starts_a_round() {
    let mut tasks = HostBackgroundTasks::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    tasks.spawn(
        "delayed",
        periodic(
            tasks.token(),
            Duration::from_secs(300),
            Duration::from_secs(3600),
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                async {}
            },
        ),
    );
    tasks.seal();
    tokio::task::yield_now().await;
    tasks.cancel();
    tasks.wait().await;
    tokio::time::advance(Duration::from_secs(7200)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn cancellation_while_waiting_for_tick_does_not_run_again() {
    let mut tasks = HostBackgroundTasks::new();
    let (called_tx, mut called_rx) = mpsc::unbounded_channel();
    tasks.spawn(
        "periodic",
        periodic(
            tasks.token(),
            Duration::ZERO,
            Duration::from_secs(60),
            move || {
                called_tx.send(()).unwrap();
                async {}
            },
        ),
    );
    tasks.seal();
    called_rx.recv().await.unwrap();
    tasks.cancel();
    tasks.wait().await;
    tokio::time::advance(Duration::from_secs(120)).await;
    assert_eq!(called_rx.recv().await, None);
}

#[tokio::test]
async fn cancellation_drains_an_admitted_cleanup_round() {
    let mut tasks = HostBackgroundTasks::new();
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (release_tx, release_rx) = oneshot::channel();
    let mut release = Some(release_rx);
    let finished = Arc::new(AtomicBool::new(false));
    let observed = finished.clone();
    tasks.spawn(
        "cleanup",
        periodic(
            tasks.token(),
            Duration::ZERO,
            Duration::from_secs(60),
            move || {
                started_tx.send(()).unwrap();
                let release = release.take().expect("no second round after cancellation");
                let finished = observed.clone();
                async move {
                    release.await.unwrap();
                    finished.store(true, Ordering::SeqCst);
                }
            },
        ),
    );
    tasks.seal();
    started_rx.recv().await.unwrap();
    tasks.cancel();
    assert!(futures::poll!(Box::pin(tasks.wait())).is_pending());
    assert!(!finished.load(Ordering::SeqCst));
    release_tx.send(()).unwrap();
    tasks.wait().await;
    assert!(finished.load(Ordering::SeqCst));
    assert_eq!(started_rx.recv().await, None);
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn memory_cancellation_discards_current_maintenance_future() {
    let mut tasks = HostBackgroundTasks::new();
    let stop = tasks.token();
    let (started_tx, started_rx) = oneshot::channel();
    let destroyed = Arc::new(AtomicBool::new(false));
    let guard = Dropped(destroyed.clone());
    tasks.spawn("memory", async move {
        until_cancelled(&stop, async move {
            let _guard = guard;
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        })
        .await;
    });
    tasks.seal();
    started_rx.await.unwrap();
    tasks.cancel();
    tasks.wait().await;
    assert!(destroyed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn backup_waiting_for_restore_lock_can_be_cancelled() {
    let mut tasks = HostBackgroundTasks::new();
    let stop = tasks.token();
    let lock = Arc::new(Mutex::new(()));
    let guard = lock.lock().await;
    let work_lock = lock.clone();
    let started = Arc::new(AtomicBool::new(false));
    let observed = started.clone();
    tasks.spawn("backup", async move {
        locked_round(&stop, &work_lock, || async move {
            observed.store(true, Ordering::SeqCst);
        })
        .await;
    });
    tasks.seal();
    tokio::task::yield_now().await;
    tasks.cancel();
    tasks.wait().await;
    assert!(!started.load(Ordering::SeqCst));
    drop(guard);
}

#[tokio::test]
async fn started_backup_keeps_restore_lock_through_blocking_work_and_result_recording() {
    let mut tasks = HostBackgroundTasks::new();
    let stop = tasks.token();
    let lock = Arc::new(Mutex::new(()));
    let work_lock = lock.clone();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_work_tx, release_work_rx) = std::sync::mpsc::channel();
    let (recording_tx, recording_rx) = oneshot::channel();
    let (release_record_tx, release_record_rx) = oneshot::channel();
    let recorded = Arc::new(AtomicBool::new(false));
    let observed = recorded.clone();
    tasks.spawn("backup", async move {
        locked_round(&stop, &work_lock, || async move {
            tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                release_work_rx.recv().unwrap();
            })
            .await
            .unwrap();
            recording_tx.send(()).unwrap();
            release_record_rx.await.unwrap();
            observed.store(true, Ordering::SeqCst);
        })
        .await;
    });
    tasks.seal();
    started_rx.await.unwrap();
    tasks.cancel();
    assert!(futures::poll!(Box::pin(tasks.wait())).is_pending());
    assert!(lock.try_lock().is_err());
    release_work_tx.send(()).unwrap();
    recording_rx.await.unwrap();
    assert!(futures::poll!(Box::pin(tasks.wait())).is_pending());
    assert!(lock.try_lock().is_err());
    assert!(!recorded.load(Ordering::SeqCst));
    release_record_tx.send(()).unwrap();
    tasks.wait().await;
    assert!(recorded.load(Ordering::SeqCst));
    assert!(lock.try_lock().is_ok());
}
