use super::*;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cancellation_wakes_waiting_tasks_and_registration_is_removed() {
    let mut scope = HostBackgroundTasks::new();
    let token = scope.token();
    let exited = Arc::new(AtomicBool::new(false));
    let task_exited = exited.clone();
    scope.spawn("idle maintenance", async move {
        token.cancelled().await;
        task_exited.store(true, Ordering::SeqCst);
    });
    scope.seal();
    scope.cancel();
    scope.wait().await;
    assert!(exited.load(Ordering::SeqCst));
    assert!(scope.active_names.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wait_covers_started_blocking_work_and_future_destruction() {
    let mut scope = HostBackgroundTasks::new();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let completed = Arc::new(AtomicBool::new(false));
    let destroyed = Arc::new(AtomicBool::new(false));
    let work_completed = completed.clone();
    let task_destroyed = destroyed.clone();
    scope.spawn("backup", async move {
        let _guard = DropFlag(task_destroyed);
        tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            work_completed.store(true, Ordering::SeqCst);
        })
        .await
        .unwrap();
    });
    scope.seal();
    started_rx.await.unwrap();
    scope.cancel();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), scope.wait())
            .await
            .is_err()
    );
    assert!(!completed.load(Ordering::SeqCst));
    assert!(!destroyed.load(Ordering::SeqCst));
    release_tx.send(()).unwrap();
    scope.wait().await;
    assert!(completed.load(Ordering::SeqCst));
    assert!(destroyed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn dropping_scope_notifies_without_discarding_started_work() {
    let mut scope = HostBackgroundTasks::new();
    let token = scope.token();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (completed_tx, completed_rx) = oneshot::channel();
    scope.spawn("backup", async move {
        started_tx.send(()).unwrap();
        release_rx.await.unwrap();
        completed_tx.send(()).unwrap();
    });
    started_rx.await.unwrap();
    drop(scope);
    assert!(token.is_cancelled());
    release_tx.send(()).unwrap();
    completed_rx.await.unwrap();
}

#[tokio::test]
#[should_panic(expected = "宿主后台任务登记已经关闭")]
async fn seal_rejects_late_registration() {
    let mut scope = HostBackgroundTasks::new();
    scope.seal();
    scope.spawn("late task", async {});
}

#[tokio::test]
#[should_panic(expected = "宿主后台任务登记已经关闭")]
async fn cancel_rejects_late_registration() {
    let mut scope = HostBackgroundTasks::new();
    scope.cancel();
    scope.spawn("late task", async {});
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn panic_is_named_in_logs_and_does_not_prevent_wait_completion() {
    let log_bytes = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(LogWriter(log_bytes.clone()))
        .with_ansi(false)
        .without_time()
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let mut scope = HostBackgroundTasks::new();
    scope.spawn("broken maintenance", async {
        panic!("maintenance failure")
    });
    scope.seal();
    scope.cancel();
    tokio::time::timeout(Duration::from_secs(1), scope.wait())
        .await
        .expect("panicking task must leave the tracker");
    assert!(scope.active_names.lock().unwrap().is_empty());
    let logs = String::from_utf8(log_bytes.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("broken maintenance"), "{logs}");
    assert!(logs.contains("maintenance failure"), "{logs}");
}
