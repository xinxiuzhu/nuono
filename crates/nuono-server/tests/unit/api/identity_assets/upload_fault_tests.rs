//! 验证各角色与各类素材（头像、品牌 Logo）的 multipart 上传路由共享同一个全局上传并发配额。
use super::*;
use axum::body::Bytes;
use sea_orm::{ConnectionTrait, Statement};
use std::{
    io::Cursor,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    task::Poll,
    time::Duration,
};

fn png_body() -> Body {
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(8, 8)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let mut body = b"--upload-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
    body.extend_from_slice(&png);
    body.extend_from_slice(b"\r\n--upload-boundary--\r\n");
    Body::from(body)
}

fn pending_body(polls: Arc<AtomicUsize>) -> Body {
    Body::from_stream(futures::stream::poll_fn(move |_| {
        polls.fetch_add(1, Ordering::SeqCst);
        Poll::<Option<Result<Bytes, std::io::Error>>>::Pending
    }))
}

fn upload_request(path: &str, token: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(
            "content-type",
            "multipart/form-data; boundary=upload-boundary",
        )
        .header("authorization", format!("Bearer {token}"))
        .header("cookie", "csrf_token=upload-csrf")
        .header("x-csrf-token", "upload-csrf")
        .body(body)
        .unwrap()
}

async fn upload(f: &Fixture, path: &str, token: &str) -> (StatusCode, Value) {
    json_response(
        f.app
            .clone()
            .oneshot(upload_request(path, token, png_body()))
            .await
            .unwrap(),
    )
    .await
}

async fn wait_for_jobs(state: &AppState, jobs: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.asset_uploads.active_jobs() != jobs {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("upload pipeline did not reach the expected active count");
}

async fn wait_for_slots(state: &AppState, slots: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.asset_uploads.available_slots() != slots {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("upload resources did not release their admission leases");
}

fn files(directory: &Path) -> Vec<String> {
    let mut files = match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("cannot read {}: {error}", directory.display()),
    };
    files.sort();
    files
}

async fn avatar_filename(f: &Fixture, username: &str) -> String {
    let user = f
        .state
        .db
        .find_user_by_username(username)
        .await
        .unwrap()
        .unwrap();
    f.state
        .db
        .get_global_config(&format!("admin_profile:{}", user.id))
        .await
        .unwrap()
        .unwrap()["avatar_filename"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn avatar_and_logo_share_capacity_and_reject_before_polling_multipart() {
    let f = Fixture::new(true).await;
    let user = f.session("avatar-user", "user").await;
    let staff = f.session("logo-owner", "owner").await;
    let first = f.state.asset_uploads.admit().unwrap();
    let second = f.state.asset_uploads.admit().unwrap();
    for (path, token, expected) in [
        (
            "/api/account/avatar",
            &user,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            "/api/branding/logo",
            &staff,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        // 路由权限校验在申请上传并发槽位之前执行：即便上传槽位已满，越权请求仍应返回 403 而非 503。
        ("/api/branding/logo", &user, StatusCode::FORBIDDEN),
    ] {
        let polls = Arc::new(AtomicUsize::new(0));
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            f.app
                .clone()
                .oneshot(upload_request(path, token, pending_body(polls.clone()))),
        )
        .await
        .expect("rejected upload tried to read its request body")
        .unwrap();
        let (status, body) = json_response(response).await;
        assert_eq!(status, expected, "{path}: {body}");
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }
    drop((first, second));
    let (status, body) = upload(&f, "/api/account/avatar", &user).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["role"], "user");
    assert!(body["data"]["avatar_url"].is_string());
    wait_for_slots(&f.state, 2).await;
    f.close().await;
}

#[tokio::test]
async fn decoded_uploads_keep_capacity_while_waiting_for_the_publication_lock() {
    let f = Fixture::new(true).await;
    let user = f.session("waiting-avatar", "user").await;
    let staff = f.session("waiting-logo", "owner").await;
    let guard = f.state.identity_assets_lock.clone().lock_owned().await;
    let avatar = tokio::spawn({
        let app = f.app.clone();
        async move {
            app.oneshot(upload_request("/api/account/avatar", &user, png_body()))
                .await
                .unwrap()
        }
    });
    let logo = tokio::spawn({
        let app = f.app.clone();
        async move {
            app.oneshot(upload_request("/api/branding/logo", &staff, png_body()))
                .await
                .unwrap()
        }
    });
    wait_for_jobs(&f.state, 2).await;
    assert!(!avatar.is_finished() && !logo.is_finished());
    assert!(matches!(
        f.state.asset_uploads.admit(),
        Err(nuono_core::NuonoError::Overloaded(_))
    ));
    drop(guard);
    for task in [avatar, logo] {
        let (status, body) = json_response(task.await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    wait_for_slots(&f.state, 2).await;
    assert!(files(&f.state.paths.identity_assets().join(".tmp")).is_empty());
    assert_eq!(
        files(&f.state.paths.identity_assets().join("profile")).len(),
        1
    );
    assert_eq!(
        files(&f.state.paths.identity_assets().join("branding")).len(),
        1
    );
    f.close().await;
}

#[tokio::test]
async fn cancelling_an_accepted_avatar_keeps_its_lease_and_finishes_publication() {
    let f = Fixture::new(true).await;
    let user = f.session("cancelled-avatar", "user").await;
    let (status, body) = upload(&f, "/api/account/avatar", &user).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let old_name = avatar_filename(&f, "cancelled-avatar").await;
    let guard = f.state.identity_assets_lock.clone().lock_owned().await;
    let request = tokio::spawn({
        let app = f.app.clone();
        async move {
            app.oneshot(upload_request("/api/account/avatar", &user, png_body()))
                .await
                .unwrap()
        }
    });
    wait_for_jobs(&f.state, 1).await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(f.state.asset_uploads.active_jobs(), 1);
    let spare = f.state.asset_uploads.admit().unwrap();
    assert!(matches!(
        f.state.asset_uploads.admit(),
        Err(nuono_core::NuonoError::Overloaded(_))
    ));
    drop(spare);
    drop(guard);
    wait_for_jobs(&f.state, 0).await;
    wait_for_slots(&f.state, 2).await;
    let name = avatar_filename(&f, "cancelled-avatar").await;
    assert_ne!(name, old_name);
    assert_eq!(
        files(&f.state.paths.identity_assets().join("profile")),
        vec![name]
    );
    assert!(files(&f.state.paths.identity_assets().join(".tmp")).is_empty());
    f.close().await;
}

#[tokio::test]
async fn failed_avatar_metadata_write_removes_new_file_and_preserves_old_asset() {
    let f = Fixture::new(true).await;
    let user = f.session("failed-avatar", "user").await;
    let (status, body) = upload(&f, "/api/account/avatar", &user).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let old_name = avatar_filename(&f, "failed-avatar").await;
    // 将配置表替换为带 `DISTINCT` 的只读视图：读操作（含真实会话认证）仍正常，但在 PostgreSQL 与 SQLite 上均会拒绝写入。
    // 注意每次 DDL 变更都在同一个事务连接内完成，避免 SQLite 连接池中的其他连接仍缓存着旧视图/表名。
    let transaction = f.state.db.begin_write().await.unwrap();
    for sql in [
        "ALTER TABLE nuono_global_configs RENAME TO upload_config_values",
        "CREATE VIEW nuono_global_configs AS SELECT DISTINCT * FROM upload_config_values",
    ] {
        transaction
            .execute_raw(Statement::from_string(f.state.db.backend(), sql))
            .await
            .unwrap();
    }
    transaction.commit().await.unwrap();
    let (status, body) = upload(&f, "/api/account/avatar", &user).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    wait_for_slots(&f.state, 2).await;
    assert_eq!(avatar_filename(&f, "failed-avatar").await, old_name);
    assert_eq!(
        files(&f.state.paths.identity_assets().join("profile")),
        vec![old_name]
    );
    assert!(files(&f.state.paths.identity_assets().join(".tmp")).is_empty());
    let transaction = f.state.db.begin_write().await.unwrap();
    for sql in [
        "DROP VIEW nuono_global_configs",
        "ALTER TABLE upload_config_values RENAME TO nuono_global_configs",
    ] {
        transaction
            .execute_raw(Statement::from_string(f.state.db.backend(), sql))
            .await
            .unwrap();
    }
    transaction.commit().await.unwrap();
    let (status, body) = upload(&f, "/api/account/avatar", &user).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_for_slots(&f.state, 2).await;
    assert_eq!(
        files(&f.state.paths.identity_assets().join("profile")).len(),
        1
    );
    f.close().await;
}

#[tokio::test]
async fn closing_uploads_interrupts_an_incomplete_avatar_body_without_publishing() {
    let f = Fixture::new(true).await;
    let user = f.session("slow-avatar", "user").await;
    let polls = Arc::new(AtomicUsize::new(0));
    let request = tokio::spawn({
        let app = f.app.clone();
        let polls = polls.clone();
        async move {
            app.oneshot(upload_request(
                "/api/account/avatar",
                &user,
                pending_body(polls),
            ))
            .await
            .unwrap()
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while polls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    f.state.asset_uploads.close();
    let response = tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .unwrap()
        .unwrap();
    let (status, body) = json_response(response).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    f.state.asset_uploads.wait().await;
    assert!(files(&f.state.paths.identity_assets().join("profile")).is_empty());
    assert!(files(&f.state.paths.identity_assets().join(".tmp")).is_empty());
    f.close().await;
}
