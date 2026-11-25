use super::{await_shutdown, run_http_phase};
use crate::background_tasks::HostBackgroundTasks;
use futures::FutureExt;
use nuono_core::core::{FrameworkControl, FrameworkControlSignal};
use std::{
    cell::Cell,
    future::pending,
    io::ErrorKind,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot};

fn waiting_scope(exited: Arc<AtomicBool>) -> HostBackgroundTasks {
    let mut tasks = HostBackgroundTasks::new();
    let stop = tasks.token();
    tasks.spawn("waiting maintenance", async move {
        stop.cancelled().await;
        exited.store(true, Ordering::SeqCst);
    });
    tasks.seal();
    tasks
}

async fn wait_for_scope(tasks: &HostBackgroundTasks) {
    tokio::time::timeout(Duration::from_secs(1), tasks.wait())
        .await
        .expect("cancelled maintenance must exit");
}

#[tokio::test]
async fn os_shutdown_notifies_background_tasks_once() {
    let exited = Arc::new(AtomicBool::new(false));
    let tasks = waiting_scope(exited.clone());
    let control = FrameworkControl::new();
    let (os_tx, os_rx) = oneshot::channel();
    let notifications = Cell::new(0);
    os_tx.send(()).unwrap();

    let signal = await_shutdown(async { os_rx.await.unwrap() }, control.subscribe(), || {
        notifications.set(notifications.get() + 1);
        tasks.cancel();
    })
    .await;

    assert_eq!(signal, None);
    assert_eq!(notifications.get(), 1);
    assert!(tasks.token().is_cancelled());
    wait_for_scope(&tasks).await;
    assert!(exited.load(Ordering::SeqCst));
}

async fn assert_framework_shutdown_preserves_signal(expected: FrameworkControlSignal) {
    let exited = Arc::new(AtomicBool::new(false));
    let tasks = waiting_scope(exited.clone());
    let control = FrameworkControl::new();
    // 生产环境在启动平台适配器前就已订阅控制信号；这里在 poll `await_shutdown` 之前先发出信号，
    // 用于验证启动剩余阶段提前入队的信号也能被正常捕获。
    let receiver = control.subscribe();
    match &expected {
        FrameworkControlSignal::Shutdown { reason } => control.request_shutdown(reason),
        FrameworkControlSignal::Restart { reason } => control.request_restart(reason),
        _ => unreachable!("测试仅覆盖关机与重启控制信号"),
    }
    let notifications = Cell::new(0);

    let signal = await_shutdown(pending(), receiver, || {
        notifications.set(notifications.get() + 1);
        tasks.cancel();
    })
    .await;

    assert_eq!(signal, Some(expected.clone()));
    assert_eq!(control.last_signal(), Some(expected));
    assert_eq!(notifications.get(), 1);
    assert!(tasks.token().is_cancelled());
    wait_for_scope(&tasks).await;
    assert!(exited.load(Ordering::SeqCst));
}

#[tokio::test]
async fn queued_framework_shutdown_cancels_tasks_and_preserves_reason() {
    assert_framework_shutdown_preserves_signal(FrameworkControlSignal::Shutdown {
        reason: "administrator shutdown during startup".to_string(),
    })
    .await;
}

#[tokio::test]
async fn queued_framework_restart_cancels_tasks_and_preserves_restart() {
    assert_framework_shutdown_preserves_signal(FrameworkControlSignal::Restart {
        reason: "restart after maintenance".to_string(),
    })
    .await;
}

#[tokio::test]
async fn occupied_listener_cancels_background_tasks_and_preserves_bind_error() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = occupied.local_addr().unwrap();
    let exited = Arc::new(AtomicBool::new(false));
    let tasks = waiting_scope(exited.clone());

    let error = run_http_phase(&tasks, async {
        let _listener = TcpListener::bind(address).await?;
        Ok(())
    })
    .await
    .expect_err("a second listener cannot bind the occupied address");

    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        ErrorKind::AddrInUse
    );
    assert!(tasks.token().is_cancelled());
    wait_for_scope(&tasks).await;
    assert!(exited.load(Ordering::SeqCst));
    drop(occupied);
}

#[tokio::test]
async fn bind_failure_notifies_without_discarding_started_maintenance() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = occupied.local_addr().unwrap();
    let mut tasks = HostBackgroundTasks::new();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let worker_finished = finished.clone();
    tasks.spawn("in-flight backup", async move {
        started_tx.send(()).unwrap();
        release_rx.await.unwrap();
        worker_finished.store(true, Ordering::SeqCst);
    });
    tasks.seal();
    started_rx.await.unwrap();

    let error = run_http_phase(&tasks, async {
        let _listener = TcpListener::bind(address).await?;
        Ok(())
    })
    .await
    .unwrap_err();

    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        ErrorKind::AddrInUse
    );
    assert!(tasks.token().is_cancelled());
    // `run_http_phase` 失败退出时只发取消通知；后续清理阶段的 `wait()` 屏障仍须等待已进入执行轮次的备份完成，不能因监听失败而直接丢弃。
    assert!(tasks.wait().now_or_never().is_none());
    assert!(!finished.load(Ordering::SeqCst));
    release_tx.send(()).unwrap();
    wait_for_scope(&tasks).await;
    assert!(finished.load(Ordering::SeqCst));
    drop(occupied);
}
