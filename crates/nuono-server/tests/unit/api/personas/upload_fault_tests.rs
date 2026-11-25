use super::*;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const BOUNDARY: &str = "persona-upload-fault";

fn multipart(revision: i64) -> Body {
    let mut image = Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(8, 8)
        .write_to(&mut image, image::ImageFormat::Png)
        .unwrap();
    let mut bytes = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"revision\"\r\n\r\n{revision}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\n").into_bytes();
    bytes.extend(image.into_inner());
    bytes.extend(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    Body::from(bytes)
}

fn request(id: &str, slot: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/ai/personas/{id}/assets/{slot}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .unwrap()
}

async fn create(f: &Fixture) -> (String, i64) {
    let (status, response) = f
        .request(
            "POST",
            "/api/ai/personas",
            json!({
                "name":"upload-fault", "system_prompt":"test"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    (
        response["data"]["id"].as_str().unwrap().into(),
        response["data"]["revision"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap(),
    )
}

async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("upload must make progress")
}

async fn accepted(f: &Fixture) {
    bounded(async {
        while f.state.asset_uploads.active_jobs() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn finished(f: &Fixture, expected_slots: usize) {
    bounded(async {
        while f.state.asset_uploads.active_jobs() != 0
            || f.state.asset_uploads.available_slots() != expected_slots
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test]
async fn persona_images_and_audio_share_global_admission_before_reading_body() {
    let f = Fixture::new(false).await;
    let (id, _) = create(&f).await;
    let first = f.state.asset_uploads.admit().unwrap();
    let second = f.state.asset_uploads.admit().unwrap();
    for slot in ["avatar", "portrait", "reference_audio"] {
        let polls = Arc::new(AtomicUsize::new(0));
        let probe = polls.clone();
        let body = Body::from_stream(futures::stream::poll_fn(move |_| {
            probe.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Pending::<Option<Result<axum::body::Bytes, std::io::Error>>>
        }));
        let response = bounded(f.app.clone().oneshot(request(&id, slot, body)))
            .await
            .unwrap();
        let (status, body) = json_response(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{slot}: {body}");
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }
    assert!(!f.state.paths.persona_assets().join(id).exists());
    drop((first, second));
    f.close().await;
}

#[tokio::test]
async fn disconnected_accepted_persona_upload_keeps_capacity_and_publishes_one_complete_file() {
    let f = Fixture::new(false).await;
    let (id, revision) = create(&f).await;
    let guard = f.state.identity_assets_lock.clone().lock_owned().await;
    let app = f.app.clone();
    let upload = request(&id, "portrait", multipart(revision));
    let task = tokio::spawn(async move { app.oneshot(upload).await });
    accepted(&f).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let other = f.state.asset_uploads.admit().unwrap();
    assert!(matches!(
        f.state.asset_uploads.admit(),
        Err(nuono_core::NuonoError::Overloaded(_))
    ));
    assert!(!f.state.paths.persona_assets().join(&id).exists());
    drop(guard);
    finished(&f, 1).await;
    let record = f.state.db.get_ai_persona(&id).await.unwrap().unwrap();
    assert_eq!(record.revision, revision + 1);
    let filename = record.portrait_filename.unwrap();
    let directory = f.state.paths.persona_assets().join(&id);
    let files = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(files, [std::ffi::OsString::from(&filename)]);
    assert!(image::open(directory.join(filename)).is_ok());
    drop(other);
    f.close().await;
}

#[tokio::test]
async fn persona_revision_conflict_returns_capacity_without_creating_orphan_files() {
    let f = Fixture::new(false).await;
    let (id, revision) = create(&f).await;
    let guard = f.state.identity_assets_lock.clone().lock_owned().await;
    let app = f.app.clone();
    let upload = request(&id, "avatar", multipart(revision + 1));
    let task = tokio::spawn(async move { app.oneshot(upload).await });
    accepted(&f).await;
    drop(guard);
    let (status, body) = json_response(bounded(task).await.unwrap().unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    finished(&f, 2).await;
    let record = f.state.db.get_ai_persona(&id).await.unwrap().unwrap();
    assert_eq!(record.revision, revision);
    assert!(record.avatar_filename.is_none());
    assert!(!f.state.paths.persona_assets().join(id).exists());
    f.close().await;
}

#[tokio::test]
async fn closing_upload_scope_rejects_persona_assets_before_disk_changes() {
    let f = Fixture::new(false).await;
    let (id, revision) = create(&f).await;
    f.state.asset_uploads.close();
    let (status, body) = json_response(
        f.app
            .clone()
            .oneshot(request(&id, "avatar", multipart(revision)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(!f.state.paths.persona_assets().join(id).exists());
    f.close().await;
}
