use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use nuono_plugin::{
    AssetLimits, ImageSource, PluginAssets, PluginHttpClient, media::resolve_image,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage-content";

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("nuono-assets-{}", uuid::Uuid::new_v4())))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    url: String,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(status: &str, body: &[u8], chunked: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/asset", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let body = body.to_vec();
    let status = status.to_string();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            let response = if chunked {
                format!(
                    "HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
                    body.len()
                )
            } else {
                format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
            };
            if stream.write_all(response.as_bytes()).await.is_ok() {
                let _ = stream.write_all(&body).await;
                if chunked {
                    let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
                }
            }
        }
    });
    Server {
        url,
        requests,
        task,
    }
}

#[tokio::test]
async fn image_bytes_files_and_urls_resolve_to_the_same_upload() {
    let directory = Directory::new();
    tokio::fs::create_dir_all(&directory.0).await.unwrap();
    let path = directory.0.join("中文 图片.png");
    tokio::fs::write(&path, PNG).await.unwrap();
    let http = PluginHttpClient::new();
    let encoded = ImageSource::bytes(PNG.to_vec())
        .into_segment()
        .await
        .unwrap()
        .to_json();
    let upload = resolve_image(encoded["data"]["file"].as_str().unwrap(), http.raw(), 1024)
        .await
        .unwrap();
    assert_eq!(upload.bytes, PNG);
    assert_eq!(upload.content_type, "image/png");
    let file_url = url::Url::from_file_path(&path).unwrap();
    assert_eq!(
        resolve_image(file_url.as_str(), http.raw(), 1024)
            .await
            .unwrap()
            .bytes,
        PNG
    );
    let served = server("200 OK", PNG, false).await;
    assert_eq!(
        resolve_image(&served.url, http.raw(), 1024)
            .await
            .unwrap()
            .bytes,
        PNG
    );
    let segment = ImageSource::path(path)
        .into_segment()
        .await
        .unwrap()
        .to_json();
    assert_eq!(segment, encoded);
    assert!(
        ImageSource::bytes(b"not-an-image".to_vec())
            .into_segment()
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_fetches_share_download_and_failed_refresh_preserves_the_file() {
    let directory = Directory::new();
    let assets = PluginAssets::new(&directory.0, PluginHttpClient::new());
    let served = server("200 OK", PNG, false).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let assets = assets.clone();
        let url = served.url.clone();
        tasks.push(tokio::spawn(async move {
            assets.fetch("icons/card.png", &url).await.unwrap()
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(served.requests.load(Ordering::SeqCst), 1);
    let first = assets.read("icons/card.png").await.unwrap();
    let cached = assets.read("icons/card.png").await.unwrap();
    assert!(Arc::ptr_eq(&first, &cached));
    let broken = server("500 Internal Server Error", b"error", false).await;
    assert!(assets.refresh("icons/card.png", &broken.url).await.is_err());
    assert_eq!(assets.read("icons/card.png").await.unwrap().as_ref(), PNG);
    assets
        .fetch("icons/card.png", "http://127.0.0.1:1/offline")
        .await
        .unwrap();
    assert!(
        tokio::fs::read_dir(directory.0.join("icons"))
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn downloads_enforce_size_even_without_content_length() {
    let directory = Directory::new();
    let assets = PluginAssets::with_limits(
        &directory.0,
        PluginHttpClient::new(),
        AssetLimits {
            max_file_bytes: 8,
            memory_cache_bytes: 0,
            download_concurrency: 1,
        },
    )
    .unwrap();
    for chunked in [false, true] {
        let served = server("200 OK", b"too-large-content", chunked).await;
        assert!(assets.fetch("large.bin", &served.url).await.is_err());
        assert!(!directory.0.join("large.bin").exists());
    }
    assert!(assets.path("../other-plugin/file").is_err());
    assert!(assets.path("/outside/file").is_err());
}

#[tokio::test]
async fn default_assets_preserve_user_files_and_cache_obeys_budget() {
    let directory = Directory::new();
    let assets = PluginAssets::with_limits(
        &directory.0,
        PluginHttpClient::new(),
        AssetLimits {
            max_file_bytes: 32,
            memory_cache_bytes: 5,
            download_concurrency: 1,
        },
    )
    .unwrap();
    assets.install("a", b"first").await.unwrap();
    assets.install("a", b"default").await.unwrap();
    assert_eq!(assets.read("a").await.unwrap().as_ref(), b"first");
    let first = assets.read("a").await.unwrap();
    assets.install("b", b"other").await.unwrap();
    assets.read("b").await.unwrap();
    assert!(!Arc::ptr_eq(&first, &assets.read("a").await.unwrap()));
    tokio::fs::write(directory.0.join("a"), b"changed")
        .await
        .unwrap();
    assert_eq!(assets.read("a").await.unwrap().as_ref(), b"changed");
    let one = assets.read("a").await.unwrap();
    let two = assets.read("a").await.unwrap();
    assert!(!Arc::ptr_eq(&one, &two));
}
