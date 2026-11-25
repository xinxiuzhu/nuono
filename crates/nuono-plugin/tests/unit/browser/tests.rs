use super::*;

#[test]
fn validates_untrusted_input_before_dispatch() {
    for url in [
        "file:///etc/passwd",
        "data:text/html,hello",
        "https://user:secret@example.com",
        "not a URL",
    ] {
        assert!(BrowserRequest::new(url).validate().is_err());
    }
    let mut request = BrowserRequest::new("https://example.com");
    assert!(request.validate().is_ok());
    request.timeout = Duration::ZERO;
    assert!(request.validate().is_err());
    request.timeout = Duration::from_secs(30);
    request.max_output_bytes = 0;
    assert!(request.validate().is_err());
    request.max_output_bytes = 1024;
    request
        .cookies
        .push("session=secret\r\nInjected: header".into());
    assert!(request.validate().is_err());
    request.cookies.clear();
    request.proxy = Some("file:///tmp/proxy".into());
    assert!(request.validate().is_err());
}

#[cfg(not(feature = "obscura"))]
#[tokio::test]
async fn missing_feature_is_an_explicit_error() {
    initialize().unwrap();
    assert!(!is_available());
    let scope = Arc::new(PluginTaskScope::new("browser-disabled"));
    let error = PluginBrowser::for_scope(scope.clone())
        .fetch(BrowserRequest::new("https://example.com"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("feature"));
    scope.shutdown().await;
}

// 由单个测试独占共享浏览器池并串行覆盖全部场景，避免多个测试并发争抢池容量断言。
// 此处无需真实 V8、网络、数据库或特性开关即可验证生命周期控制。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_keeps_running_work_bounded_and_shutdown_joins_it() {
    let scope = Arc::new(PluginTaskScope::new("browser-lifecycle"));
    let browser = PluginBrowser::for_scope(scope.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker_browser = browser.clone();
    let caller = tokio::spawn(async move {
        worker_browser
            .run(Instant::now() + Duration::from_millis(100), move |_| {
                let _ = started_tx.send(());
                // 模拟同步页面导航在调用方超时放弃后仍在后台继续执行的情形。
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(())
            })
            .await
    });
    started_rx.await.unwrap();
    assert!(caller.await.unwrap().is_err());
    assert_eq!(BROWSER_POOL.slots.available_permits(), 0);

    let mut queued = Vec::new();
    for _ in 0..8 {
        let browser = browser.clone();
        queued.push(tokio::spawn(async move {
            browser
                .run::<(), _>(Instant::now() + Duration::from_secs(5), |_| {
                    panic!("queued work must not run after shutdown")
                })
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while BROWSER_POOL.admitted.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let overflow = browser
        .run(Instant::now() + Duration::from_secs(1), |_| Ok(()))
        .await;
    assert!(overflow.unwrap_err().to_string().contains("任务较多"));

    let shutdown_scope = scope.clone();
    let shutdown = tokio::spawn(async move { shutdown_scope.shutdown().await });
    for waiter in queued {
        assert!(waiter.await.unwrap().is_err());
    }
    assert!(
        !shutdown.is_finished(),
        "shutdown must join the actual worker"
    );
    release_tx.send(()).unwrap();
    shutdown.await.unwrap();
    assert_eq!(BROWSER_POOL.slots.available_permits(), 1);
    assert_eq!(BROWSER_POOL.admitted.available_permits(), 9);
    assert!(
        browser
            .fetch(BrowserRequest::new("https://example.com"))
            .await
            .is_err()
    );

    let scope = Arc::new(PluginTaskScope::new("browser-caller-drop"));
    let browser = PluginBrowser::for_scope(scope.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let caller = tokio::spawn(async move {
        browser
            .run(Instant::now() + Duration::from_secs(5), move |cancelled| {
                let _ = started_tx.send(());
                let until = Instant::now() + Duration::from_secs(3);
                while !cancelled() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = cancelled_tx.send(cancelled());
                Ok(())
            })
            .await
    });
    started_rx.await.unwrap();
    caller.abort();
    let _ = caller.await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), cancelled_rx)
            .await
            .unwrap()
            .unwrap()
    );
    scope.shutdown().await;
    assert_eq!(BROWSER_POOL.slots.available_permits(), 1);
}
