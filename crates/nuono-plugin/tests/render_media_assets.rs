use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, extract::State, http::StatusCode, routing::get};
use nuono_plugin::{
    AssetLimits, ImageSource, PluginAssets, PluginHttpClient, PluginTaskScope, RenderLimits,
    RenderPool,
};

struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "nuono-assets-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        )))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn cancelled_waiter_keeps_running_render_counted_and_shutdown_waits_for_it() {
    let pool = Arc::new(
        RenderPool::new(RenderLimits {
            concurrency: 1,
            queue_capacity: 0,
        })
        .unwrap(),
    );
    let scope = Arc::new(PluginTaskScope::with_render_pool("one", pool.clone()));
    let other = PluginTaskScope::with_render_pool("two", pool);
    let (start, started) = tokio::sync::oneshot::channel();
    let (finish, finished) = std::sync::mpsc::channel();
    let caller = tokio::spawn({
        let scope = scope.clone();
        async move {
            scope
                .render("card", move || {
                    start.send(()).unwrap();
                    finished.recv().unwrap();
                    Ok(())
                })
                .await
        }
    });
    started.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    // 一个插件取消等待，另一个插件也不能越过进程共享的并发上限。
    assert!(
        other
            .render("second", || Ok(()))
            .await
            .unwrap_err()
            .to_string()
            .contains("稍后重试")
    );
    let shutdown = tokio::spawn({
        let scope = scope.clone();
        async move { scope.shutdown().await }
    });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    finish.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(scope.is_closed());
    assert!(scope.render("closed", || Ok(())).await.is_err());
    assert!(other.render("after", || Ok(7)).await.unwrap() == 7);
}

#[tokio::test]
async fn stopping_plugin_cancels_queued_render_without_running_it() {
    let pool = Arc::new(
        RenderPool::new(RenderLimits {
            concurrency: 1,
            queue_capacity: 1,
        })
        .unwrap(),
    );
    let running = Arc::new(PluginTaskScope::with_render_pool("running", pool.clone()));
    let waiting = Arc::new(PluginTaskScope::with_render_pool("waiting", pool));
    let (start, started) = tokio::sync::oneshot::channel();
    let (finish, finished) = std::sync::mpsc::channel();
    let first = tokio::spawn({
        let scope = running.clone();
        async move {
            scope
                .render("first", move || {
                    start.send(()).unwrap();
                    finished.recv().unwrap();
                    Ok(())
                })
                .await
        }
    });
    started.await.unwrap();
    let second = tokio::spawn({
        let scope = waiting.clone();
        async move {
            scope
                .render::<(), _>("queued", || panic!("已停用的排队任务不能执行"))
                .await
        }
    });
    tokio::task::yield_now().await;
    waiting.shutdown().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    finish.send(()).unwrap();
    first.await.unwrap().unwrap();
    running.shutdown().await;
}

#[tokio::test]
async fn render_panic_is_an_error_and_releases_the_slot() {
    let scope = PluginTaskScope::with_render_pool(
        "panic",
        Arc::new(
            RenderPool::new(RenderLimits {
                concurrency: 1,
                queue_capacity: 0,
            })
            .unwrap(),
        ),
    );
    let error = scope
        .render::<(), _>("bad", || panic!("fixture"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("panic"));
    assert_eq!(scope.render("good", || Ok(42)).await.unwrap(), 42);
    scope.shutdown().await;
}

#[tokio::test]
async fn disk_changes_invalidate_cache_and_memory_budget_evicts_old_bytes() {
    let dir = Directory::new();
    let assets = PluginAssets::with_limits(
        &dir.0,
        PluginHttpClient::new(),
        AssetLimits {
            max_file_bytes: 10,
            memory_cache_bytes: 6,
            download_concurrency: 1,
        },
    )
    .unwrap();
    assets.install("icons/a", b"aaaa").await.unwrap();
    let a = assets.read("icons/a").await.unwrap();
    assert!(Arc::ptr_eq(&a, &assets.read("icons/a").await.unwrap()));
    assets.install("icons/b", b"bbbbbb").await.unwrap();
    let _b = assets.read("icons/b").await.unwrap();
    assert!(!Arc::ptr_eq(&a, &assets.read("icons/a").await.unwrap()));
    tokio::fs::write(assets.path("icons/a").unwrap(), b"changed")
        .await
        .unwrap();
    assert_eq!(&*assets.read("icons/a").await.unwrap(), b"changed");
    assets.install("icons/a", b"default").await.unwrap();
    assert_eq!(&*assets.read("icons/a").await.unwrap(), b"changed");
    assert!(assets.path("../outside").is_err());
    assert!(assets.install("big", b"12345678901").await.is_err());
}

#[tokio::test]
async fn fetch_deduplicates_and_failed_refresh_preserves_file() {
    async fn download(State(count): State<Arc<AtomicUsize>>) -> &'static str {
        count.fetch_add(1, Ordering::SeqCst);
        "asset"
    }
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/asset", get(download))
        .route(
            "/bad",
            get(|| async { (StatusCode::NOT_FOUND, "not an asset") }),
        )
        .route("/big", get(|| async { "too much content" }))
        .with_state(count.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = Directory::new();
    let assets = PluginAssets::with_limits(
        &dir.0,
        PluginHttpClient::new(),
        AssetLimits {
            max_file_bytes: 10,
            memory_cache_bytes: 10,
            download_concurrency: 1,
        },
    )
    .unwrap();
    let asset_url = format!("{url}/asset");
    let (one, two) = tokio::join!(assets.fetch("a", &asset_url), assets.fetch("a", &asset_url));
    assert_eq!(one.unwrap(), two.unwrap());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(assets.refresh("a", &format!("{url}/bad")).await.is_err());
    assert!(assets.refresh("a", &format!("{url}/big")).await.is_err());
    assert_eq!(&*assets.read("a").await.unwrap(), b"asset");
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    server.abort();
}

#[tokio::test]
async fn image_paths_are_read_and_bad_content_is_rejected() {
    let dir = Directory::new();
    tokio::fs::create_dir_all(&dir.0).await.unwrap();
    let path = dir.0.join("带 空格.png");
    let bytes = b"\x89PNG\r\n\x1a\nfixture";
    tokio::fs::write(&path, bytes).await.unwrap();
    let segment = ImageSource::path(&path).into_segment().await.unwrap();
    let source = segment.to_json()["data"]["file"]
        .as_str()
        .unwrap()
        .to_owned();
    let upload =
        nuono_plugin::media::resolve_image(&source, PluginHttpClient::new().raw(), bytes.len())
            .await
            .unwrap();
    assert_eq!(upload.bytes, bytes);
    assert_eq!(upload.content_type, "image/png");
    assert!(
        nuono_plugin::media::resolve_image(&source, PluginHttpClient::new().raw(), bytes.len() - 1)
            .await
            .is_err()
    );
    let file_url = url::Url::from_file_path(&path).unwrap();
    assert_eq!(
        nuono_plugin::media::resolve_image(
            file_url.as_str(),
            PluginHttpClient::new().raw(),
            bytes.len()
        )
        .await
        .unwrap()
        .bytes,
        bytes
    );
    assert!(
        ImageSource::bytes(b"error page".to_vec())
            .into_segment()
            .await
            .is_err()
    );
    assert!(
        ImageSource::url("file:///tmp/a")
            .into_segment()
            .await
            .is_err()
    );
}
