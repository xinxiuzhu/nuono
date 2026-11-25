//! 停机时的 HTTP 连接排空超时控制（仅限制网络响应排空时长；已受理的数据库文件操作由 `bootstrap` 独立等待完成）。

use std::{
    future::{Future, IntoFuture},
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::oneshot;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// 包装底层 TCP 监听器：当排空超时到达时，可主动切断所有已建立的客户端连接（包括已脱离 Axum 常规等待范围的升级后 WebSocket 连接）。
pub(super) struct DrainListener {
    inner: TcpListener,
    connections: CancellationToken,
}

impl DrainListener {
    pub(super) fn new(inner: TcpListener, connections: CancellationToken) -> Self {
        Self { inner, connections }
    }
}

impl axum::serve::Listener for DrainListener {
    type Io = DrainIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (inner, address) = axum::serve::Listener::accept(&mut self.inner).await;
        (
            DrainIo {
                inner,
                cancelled: Box::pin(self.connections.clone().cancelled_owned()),
            },
            address,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

pub(super) struct DrainIo {
    inner: TcpStream,
    cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl DrainIo {
    fn closing(&mut self, cx: &mut Context<'_>) -> bool {
        // Poll `cancelled` Future 会将当前连接任务的 Waker 注册到取消令牌上，
        // 因此一旦触发取消，正在等待网络 I/O 的连接也会立即被唤醒并关闭。
        self.cancelled.as_mut().poll(cx).is_ready()
    }
}

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "HTTP drain deadline reached",
    )
}

impl AsyncRead for DrainIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.closing(cx) {
            return Poll::Ready(Err(closed()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for DrainIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.closing(cx) {
            return Poll::Ready(Err(closed()));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.closing(cx) {
            return Poll::Ready(Err(closed()));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.closing(cx) {
            return Poll::Ready(Err(closed()));
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.closing(cx) {
            return Poll::Ready(Err(closed()));
        }
        Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DrainOutcome {
    Completed,
    DeadlineExceeded,
}

pub(super) async fn wait_for_drain(
    serve: impl IntoFuture<Output = io::Result<()>>,
    shutdown_started: oneshot::Receiver<()>,
    budget: Duration,
    connections: CancellationToken,
) -> io::Result<DrainOutcome> {
    let _close_connections = connections.clone().drop_guard();
    let serve = serve.into_future();
    tokio::pin!(serve);

    // 正常运行期间不设总时长限制；只有当优雅停机钩子通知服务器已停止接受新连接后，才启动排空倒计时。
    tokio::select! {
        biased;
        result = &mut serve => return result.map(|()| DrainOutcome::Completed),
        shutdown = shutdown_started => {
            if shutdown.is_err() {
                // `serve` 可能在进入停机钩子前就已经异常退出；此时应直接透出原始错误，而非伪造超时。
                return serve.await.map(|()| DrainOutcome::Completed);
            }
        }
    }

    match tokio::time::timeout(budget, &mut serve).await {
        Ok(result) => result.map(|()| DrainOutcome::Completed),
        Err(_) => {
            connections.cancel();
            tracing::warn!(
                timeout_seconds = budget.as_secs_f64(),
                "HTTP 连接排空超时，继续等待关键数据库文件操作完成"
            );
            // 触发取消令牌以唤醒并强制断开所有已接受的 Hyper / WebSocket 底层连接；
            // 已受理的数据库备份与迁移任务不受影响，继续由 `DatabaseOperations::wait` 等待收尾。
            Ok(DrainOutcome::DeadlineExceeded)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::{Router, extract::ConnectInfo, routing::get, serve::ListenerExt};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct PendingBody {
        started: bool,
        dropped: Arc<AtomicBool>,
    }

    impl futures::Stream for PendingBody {
        type Item = io::Result<Bytes>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if !self.started {
                self.started = true;
                Poll::Ready(Some(Ok(Bytes::from_static(b"ready"))))
            } else {
                Poll::Pending
            }
        }
    }

    impl Drop for PendingBody {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    struct Dropped(Arc<AtomicBool>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    async fn pending_response(dropped: Arc<AtomicBool>) -> io::Result<()> {
        let _dropped = Dropped(dropped);
        let body = Body::from_stream(futures::stream::pending::<io::Result<Bytes>>());
        axum::body::to_bytes(body, usize::MAX)
            .await
            .map(|_| ())
            .map_err(io::Error::other)
    }

    #[tokio::test(start_paused = true)]
    async fn pending_response_is_only_timed_out_after_shutdown_begins() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(wait_for_drain(
            pending_response(dropped.clone()),
            stopped,
            Duration::from_secs(10),
            CancellationToken::new(),
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(
            !server.is_finished(),
            "normal service uptime has no drain deadline"
        );
        assert!(!dropped.load(Ordering::Acquire));

        stop.send(()).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(
            !server.is_finished(),
            "existing responses get the entire drain budget"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            server.await.unwrap().unwrap(),
            DrainOutcome::DeadlineExceeded
        );
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test(start_paused = true)]
    async fn normal_response_can_finish_during_the_drain_budget() {
        let (stop, stopped) = oneshot::channel();
        let (complete, completed) = oneshot::channel();
        let server = tokio::spawn(wait_for_drain(
            async move { completed.await.unwrap() },
            stopped,
            Duration::from_secs(10),
            CancellationToken::new(),
        ));
        stop.send(()).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        complete.send(Ok(())).unwrap();
        assert_eq!(server.await.unwrap().unwrap(), DrainOutcome::Completed);
    }

    #[tokio::test]
    async fn serve_error_before_shutdown_preserves_the_original_error() {
        let (_stop, stopped) = oneshot::channel();
        let error = wait_for_drain(
            async { Err(io::Error::new(io::ErrorKind::AddrInUse, "listener fault")) },
            stopped,
            Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(error.to_string(), "listener fault");
    }

    #[tokio::test(start_paused = true)]
    async fn serve_error_during_drain_preserves_the_original_error() {
        let (stop, stopped) = oneshot::channel();
        let (complete, completed) = oneshot::channel();
        let server = tokio::spawn(wait_for_drain(
            async move { completed.await.unwrap() },
            stopped,
            Duration::from_secs(10),
            CancellationToken::new(),
        ));
        stop.send(()).unwrap();
        tokio::task::yield_now().await;
        complete
            .send(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "serve fault",
            )))
            .unwrap();
        let error = server.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "serve fault");
    }

    #[tokio::test(start_paused = true)]
    async fn http_deadline_does_not_abort_or_skip_confirmed_database_writes() {
        let operations = crate::database_operations::DatabaseOperations::new();
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let (started, ready) = oneshot::channel();
        let (commit, committed) = oneshot::channel();
        let written = Arc::new(AtomicBool::new(false));
        let task_written = written.clone();
        let task_operations = operations.clone();
        let task_lock = lock.clone();
        let request = tokio::spawn(async move {
            task_operations
                .run(task_lock, async move {
                    started.send(()).unwrap();
                    committed.await.unwrap();
                    task_written.store(true, Ordering::Release);
                })
                .await
        });
        ready.await.unwrap();
        operations.close();

        let (stop, stopped) = oneshot::channel();
        stop.send(()).unwrap();
        assert_eq!(
            wait_for_drain(
                pending_response(Arc::new(AtomicBool::new(false))),
                stopped,
                Duration::from_millis(10),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
            DrainOutcome::DeadlineExceeded
        );
        assert!(!written.load(Ordering::Acquire));
        assert!(
            lock.try_lock().is_err(),
            "confirmed work still owns the database file lock"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), operations.wait())
                .await
                .is_err()
        );
        assert!(!request.is_finished());

        commit.send(()).unwrap();
        operations.wait().await;
        request.await.unwrap().unwrap();
        assert!(written.load(Ordering::Acquire));
        assert!(lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn loopback_pending_body_closes_at_deadline_while_database_barrier_still_waits() {
        let operations = crate::database_operations::DatabaseOperations::new();
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let (started, ready) = oneshot::channel();
        let (commit, committed) = oneshot::channel();
        let written = Arc::new(AtomicBool::new(false));
        let task_operations = operations.clone();
        let task_lock = lock.clone();
        let task_written = written.clone();
        let request = tokio::spawn(async move {
            task_operations
                .run(task_lock, async move {
                    started.send(()).unwrap();
                    committed.await.unwrap();
                    task_written.store(true, Ordering::Release);
                })
                .await
        });
        ready.await.unwrap();

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let body_dropped = Arc::new(AtomicBool::new(false));
        let handler_dropped = body_dropped.clone();
        let app = Router::new().route(
            "/",
            get(move |ConnectInfo(peer): ConnectInfo<SocketAddr>| {
                let dropped = handler_dropped.clone();
                async move {
                    (
                        [("x-peer", peer.to_string())],
                        Body::from_stream(PendingBody {
                            started: false,
                            dropped,
                        }),
                    )
                }
            }),
        );
        let connections = CancellationToken::new();
        let listener = DrainListener::new(listener, connections.clone()).tap_io(|_| {});
        let (stop, stopped) = oneshot::channel();
        let (drain_started, draining) = oneshot::channel();
        let shutdown_operations = operations.clone();
        let serve = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            stopped.await.unwrap();
            shutdown_operations.close();
            drain_started.send(()).unwrap();
        });
        let server = tokio::spawn(wait_for_drain(
            serve,
            draining,
            Duration::from_millis(30),
            connections,
        ));

        let mut client = TcpStream::connect(address).await.unwrap();
        let peer = client.local_addr().unwrap().to_string();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), async {
            let mut response = Vec::new();
            let mut buffer = [0; 1024];
            while !response.windows(5).any(|window| window == b"ready") {
                let count = client.read(&mut buffer).await.unwrap();
                assert!(count > 0, "response must begin before shutdown");
                response.extend_from_slice(&buffer[..count]);
            }
            response
        })
        .await
        .unwrap();
        assert!(
            String::from_utf8_lossy(&response).contains(&format!("x-peer: {peer}")),
            "the listener must preserve ConnectInfo<SocketAddr>"
        );
        stop.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            DrainOutcome::DeadlineExceeded,
        );
        let closed =
            tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut Vec::new()))
                .await
                .expect("the underlying socket must close, not only the serve waiter");
        if let Err(error) = closed {
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                ),
                "{error}"
            );
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while !body_dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Hyper must drop the pending response body");

        assert!(!written.load(Ordering::Acquire));
        assert!(lock.try_lock().is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), operations.wait())
                .await
                .is_err()
        );
        commit.send(()).unwrap();
        operations.wait().await;
        request.await.unwrap().unwrap();
        assert!(written.load(Ordering::Acquire));
        assert!(lock.try_lock().is_ok());
    }
}
