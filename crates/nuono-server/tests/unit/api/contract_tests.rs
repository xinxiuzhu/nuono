//! 使用隔离的数据库与运行时状态，对实际挂载的 HTTP 路由进行接口契约测试。
#[path = "account_merge.rs"]
mod account_merge;
#[path = "identity_assets/upload_fault_tests.rs"]
mod asset_upload_fault_tests;
#[path = "assistant.rs"]
mod assistant;
#[path = "companion.rs"]
mod companion;
#[path = "companion_experience.rs"]
mod companion_experience;
#[path = "group_authorizations.rs"]
mod group_authorizations;
#[path = "personas/upload_fault_tests.rs"]
mod persona_upload_fault_tests;
#[path = "plugin_roles.rs"]
mod plugin_roles;
#[path = "system_control.rs"]
mod system_control;
#[path = "web_chat.rs"]
mod web_chat;

use super::*;
use axum::{
    Extension,
    body::{Body, to_bytes},
    http::Request,
    middleware,
};
use nuono_adapter_fixture::FixtureAdapter as OneBotAdapter;
use nuono_core::{
    config::{ConfigManager, NuonoConfig, PluginConfigManager},
    core::{ConnectionManager, Paths},
    db::NewUser,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock, atomic::AtomicBool};
use tokio::sync::Mutex;
use tower::ServiceExt;

pub(crate) struct Fixture {
    app: Router,
    pub(crate) state: AppState,
    _root: tempfile::TempDir,
}
impl Fixture {
    pub(crate) async fn new(auth: bool) -> Self {
        Self::build(auth, false).await
    }
    pub(crate) async fn prepared(auth: bool) -> Self {
        Self::build(auth, true).await
    }
    async fn build(auth: bool, prepared: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(root.path());
        std::fs::create_dir_all(&paths.data).unwrap();
        let db = nuono_core::test_support::test_db(4).await;
        let mut cfg = NuonoConfig::default();
        cfg.security.admin_auth_enabled = auth;
        let config_path = root.path().join("nuono.toml");
        std::fs::write(&config_path, toml::to_string(&cfg).unwrap()).unwrap();
        let config = Arc::new(ConfigManager::load(Some(config_path)).await.unwrap());
        config.attach_database(db.clone()).await.unwrap();
        let plugin_config = Arc::new(PluginConfigManager::with_database(
            paths.data.clone(),
            db.clone(),
        ));
        let manager = if prepared {
            ConnectionManager::new_prepared(
                db.clone(),
                paths.clone(),
                Arc::new(OneBotAdapter),
                config.clone(),
                plugin_config.clone(),
            )
            .await
        } else {
            ConnectionManager::new(
                db.clone(),
                paths.clone(),
                Arc::new(OneBotAdapter),
                config.clone(),
                plugin_config.clone(),
            )
            .await
        }
        .unwrap();
        let adapter_manager = Arc::new(nuono_core::adapter::AdapterManager::new());
        let state = AppState {
            db: db.clone(),
            paths,
            operations: crate::operations::OperationsMonitor::new(),
            identity_assets_lock: Arc::new(Mutex::new(())),
            asset_uploads: crate::asset_uploads::AssetUploads::new(),
            plugin_page_sessions: crate::api::plugin_pages::PageSessions::new(),
            restore_lock: Arc::new(Mutex::new(())),
            database_operations: crate::database_operations::DatabaseOperations::new(),
            config,
            plugin_config,
            rate_limiter: Arc::new(crate::middleware::RateLimiter::new()),
            listen_port: Arc::new(OnceLock::new()),
            bot_instances: Arc::new(crate::bot_instances::BotInstanceRuntimeManager::new(
                db,
                adapter_manager.clone(),
                manager.event_kernel.clone(),
            )),
            sandbox: crate::sandbox::SandboxManager::new(
                manager.event_kernel.clone(),
                manager.wait_manager.clone(),
            ),
            manager,
            adapter_manager,
            readiness: Arc::new(AtomicBool::new(true)),
            business_startup: crate::business_startup::BusinessStartup::new(),
            started_at: std::time::Instant::now(),
            prometheus: metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle(),
        };
        let mut app = super::router().fallback(super::not_found);
        if auth {
            app = app.layer(middleware::from_fn_with_state(
                state.clone(),
                crate::middleware::security_middleware,
            ));
        } else {
            app = app.layer(Extension(crate::middleware::CallerIdentity::new(
                1,
                "fixture",
                nuono_core::auth::Role::Owner,
            )));
        }
        Self {
            app: app.with_state(state.clone()),
            state,
            _root: root,
        }
    }
    pub(crate) async fn close(self) {
        self.state.business_startup.close();
        self.state.manager.request_business_shutdown();
        self.state.asset_uploads.close();
        self.state.asset_uploads.wait().await;
        self.state.database_operations.close();
        self.state.database_operations.wait().await;
        self.state.manager.scheduler.shutdown().await.unwrap();
        self.state
            .manager
            .event_kernel
            .agent_runtime
            .shutdown()
            .await
            .unwrap();
        self.state.manager.plugin_host.shutdown().await.unwrap();
        self.state.db.shutdown().await.unwrap();
    }
    pub(crate) async fn request(
        &self,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        json_response(
            self.app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await
    }
    pub(crate) async fn session(&self, username: &str, role: &str) -> String {
        let user = self
            .state
            .db
            .create_user(NewUser {
                username: username.into(),
                email: None,
                password: "correct-horse-battery".into(),
                role: role.into(),
                status: "active".into(),
                email_verified: true,
            })
            .await
            .unwrap();
        let token = format!("fixture-{username}");
        assert!(
            self.state
                .db
                .create_session_for_verified_user(
                    &user,
                    &format!(
                        "auth_session_{}",
                        hex::encode(Sha256::digest(token.as_bytes()))
                    ),
                    chrono::Utc::now() + chrono::Duration::days(1)
                )
                .await
                .unwrap()
        );
        token
    }

    async fn authenticated(&self, path: &str, token: &str) -> (StatusCode, Value) {
        json_response(
            self.app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await
    }
    async fn authenticated_json(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        // 写请求要过 CSRF 比对：Cookie 与 Header 各带一份相同的值。不补这两个头的话
        // 所有写请求都会 403，断言就分不清「能力不足」和「CSRF 没带」。
        const CSRF: &str = "fixture-csrf";
        json_response(
            self.app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("content-type", "application/json")
                        .header("authorization", format!("Bearer {token}"))
                        .header("x-csrf-token", CSRF)
                        .header("cookie", format!("csrf_token={CSRF}"))
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await
    }
}
async fn json_response(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    assert_eq!(response.headers()["content-type"], "application/json");
    let bytes = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    if !status.is_success() {
        assert_eq!(value["success"], false);
        assert!(value["data"].is_null());
        assert!(value["error"].is_string());
    }
    (status, value)
}

#[tokio::test]
async fn legacy_routes_distinguish_bad_input_missing_state_and_conflicts() {
    let f = Fixture::new(false).await;
    let mut failures = Vec::new();
    for (method, path, body, expected) in [
        (
            "POST",
            "/api/tasks",
            json!({"name":"invalid cron","cron_expr":"not cron","action_type":"send_private_message","target_id":"1","content":"hello"}),
            400,
        ),
        (
            "POST",
            "/api/tasks",
            json!({"name":"invalid target","cron_expr":"* * * * * *","action_type":"send_group_message","target_id":"bad","content":"hello"}),
            400,
        ),
        (
            "POST",
            "/api/tasks",
            json!({"name":"invalid bot","cron_expr":"* * * * * *","action_type":"push_subscribe","action_params":"{\"topic\":\"news\",\"message\":\"hello\"}","bot_id":"bad"}),
            400,
        ),
        (
            "POST",
            "/api/tasks",
            json!({"name":"test","cron_expr":"0 0 0 * * *","action_type":"unsupported"}),
            400,
        ),
        (
            "POST",
            "/api/tasks/2147483647/toggle",
            json!({"enabled":true}),
            404,
        ),
        (
            "POST",
            "/api/subscriptions/2147483647/toggle",
            json!({"enabled":true}),
            404,
        ),
        (
            "POST",
            "/api/tasks/not-a-number/toggle",
            json!({"enabled":true}),
            400,
        ),
        ("GET", "/api/logs?limit=not-a-number", Value::Null, 400),
        (
            "GET",
            "/api/plugins/not-installed/config/status",
            Value::Null,
            404,
        ),
        (
            "POST",
            "/api/commands/not-installed/toggle",
            json!({"enabled":true}),
            404,
        ),
        ("POST", "/api/store/install/invalid!", json!({}), 400),
        (
            "POST",
            "/api/store/install",
            json!({"repo_url":"file:///tmp/bad"}),
            400,
        ),
        (
            "POST",
            "/api/store/install",
            json!({"repo_url":"https://github.com/xinxiuzhu/nuono-adapter-qq.git","id":"hub"}),
            400,
        ),
        ("POST", "/api/store/jobs/not-a-uuid/retry", Value::Null, 400),
        (
            "POST",
            "/api/store/jobs/00000000-0000-0000-0000-000000000000/retry",
            Value::Null,
            404,
        ),
        (
            "POST",
            "/api/store/settings",
            json!({"index_url":"http://insecure.example/catalog.json"}),
            400,
        ),
        ("POST", "/api/ai/chat", json!({"messages":[]}), 409),
        (
            "POST",
            "/api/ai/chat/stream",
            json!({"model_id":2147483647,"messages":[]}),
            404,
        ),
        ("POST", "/api/ai/chat/stream", json!({"messages":[]}), 409),
        (
            "GET",
            "/api/ai/personas/00000000-0000-0000-0000-000000000000",
            Value::Null,
            404,
        ),
        (
            "GET",
            "/api/database/tables/unknown_table/schema",
            Value::Null,
            404,
        ),
        (
            "DELETE",
            "/api/upload/wallpapers/missing.png",
            Value::Null,
            404,
        ),
        ("GET", "/api/does-not-exist", Value::Null, 404),
        ("PUT", "/api/stats", Value::Null, 405),
    ] {
        let (status, body) = f.request(method, path, body).await;
        if status.as_u16() != expected {
            failures.push(format!(
                "{method} {path}: expected {expected}, got {status}: {body}"
            ));
        }
    }
    let response = crate::http_server::router(f.state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/missing-route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_response(response).await.0, StatusCode::NOT_FOUND);
    assert!(
        nuono_core::service::tasks::list(&f.state.db)
            .await
            .unwrap()
            .is_empty()
    );
    f.close().await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn backup_and_transfer_errors_preserve_files_and_use_json() {
    let f = Fixture::new(false).await;
    let name = "nuono_manual_backup_fixture.dump";
    let directory = f.state.paths.data.join(crate::backup::BACKUP_DIR);
    std::fs::create_dir_all(&directory).unwrap();
    let backup = directory.join(name);
    std::fs::write(&backup, b"SQLite format 3\0").unwrap();
    for (method, path, body, expected) in [
        (
            "GET",
            "/api/database/backups/not-a-backup/download".to_string(),
            Value::Null,
            400,
        ),
        (
            "GET",
            "/api/database/backups/nuono_manual_backup_missing.dump/download".to_string(),
            Value::Null,
            404,
        ),
        (
            "DELETE",
            format!("/api/database/backups/{name}"),
            json!({"confirmation":"wrong"}),
            400,
        ),
        (
            "GET",
            "/api/database/transfers/invalid/download".into(),
            Value::Null,
            400,
        ),
        (
            "GET",
            "/api/database/transfers/00000000-0000-0000-0000-000000000000/download".into(),
            Value::Null,
            404,
        ),
        (
            "GET",
            "/api/database/transfers/00000000-0000-0000-0000-000000000000".into(),
            Value::Null,
            404,
        ),
    ] {
        let (status, body) = f.request(method, &path, body).await;
        assert_eq!(status.as_u16(), expected, "{method} {path}: {body}");
    }
    std::fs::write(
        f.state.paths.data.join(crate::backup::RESTORE_MARKER),
        serde_json::to_vec(&crate::backup::RestoreMarker {
            source: std::fs::canonicalize(&backup)
                .unwrap()
                .to_string_lossy()
                .into(),
            actor: "test".into(),
        })
        .unwrap(),
    )
    .unwrap();
    let (status, _) = f
        .request(
            "DELETE",
            &format!("/api/database/backups/{name}"),
            json!({"confirmation":name}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(backup.is_file());
    assert_eq!(
        f.request("POST", "/api/database/transfers", Value::Null)
            .await
            .0,
        StatusCode::CONFLICT
    );
    f.close().await;
}

#[tokio::test]
async fn malformed_and_oversized_uploads_never_report_success_or_create_a_file() {
    let f = Fixture::new(false).await;
    for (content_type,body,expected) in [
        ("application/json",b"{}".to_vec(),400),
        ("multipart/form-data; boundary=fixture",b"broken-form".to_vec(),400),
        ("multipart/form-data; boundary=fixture",b"--fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhi\r\n--fixture--\r\n".to_vec(),415),
        ("multipart/form-data; boundary=fixture",[b"--fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec(),vec![b'x';10*1024*1024+1],b"\r\n--fixture--\r\n".to_vec()].concat(),413),
    ] {
        let response=f.app.clone().oneshot(Request::builder().method("POST").uri("/api/upload/wallpaper").header("content-type",content_type).body(Body::from(body)).unwrap()).await.unwrap();
        let (status,body)=json_response(response).await;
        assert_eq!(status.as_u16(),expected,"{body}");
    }
    assert!(
        std::fs::read_dir(f.state.paths.wallpapers())
            .unwrap()
            .next()
            .is_none()
    );
    // 服务端落盘失败（500）必须与客户端输入格式错误（400）区分开，且错误响应中不得泄露服务器本地文件路径。
    std::fs::remove_dir(f.state.paths.wallpapers()).unwrap();
    std::fs::write(f.state.paths.wallpapers(), b"not a directory").unwrap();
    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/upload/wallpaper")
                .header("content-type", "multipart/form-data; boundary=x")
                .body(Body::from("--x--\r\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = json_response(response).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!body.to_string().contains(f._root.path().to_str().unwrap()));
    f.close().await;
}

#[tokio::test]
async fn authentication_outage_is_not_an_expired_session_or_permission_denial() {
    use sea_orm::ConnectionTrait;
    let f = Fixture::new(true).await;
    let token = f.session("member", "user").await;
    assert_eq!(
        f.request("GET", "/api/tasks", Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.authenticated("/api/tasks", "expired").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body) = f.authenticated("/api/tasks", &token).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!body["error"].as_str().unwrap().contains("服务日志"));
    f.state
        .db
        .orm()
        .execute_unprepared("ALTER TABLE nuono_sessions RENAME TO fixture_unavailable_sessions")
        .await
        .unwrap();
    for path in ["/api/tasks", "/api/auth/status", "/api/notifications"] {
        let (status, body) = f.authenticated(path, &token).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{path}: {body}");
        assert!(!body.to_string().contains("nuono_sessions"));
    }
    f.state
        .db
        .orm()
        .execute_unprepared("ALTER TABLE fixture_unavailable_sessions RENAME TO nuono_sessions")
        .await
        .unwrap();
    assert_eq!(
        f.authenticated("/api/auth/status", &token).await.1["data"]["authenticated"],
        true
    );
    f.close().await;
}

#[tokio::test]
async fn upstream_failures_are_sanitized_in_json_and_in_committed_sse_streams() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = Fixture::new(false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut bytes = [0u8; 8192];
            let _ = socket.read(&mut bytes).await;
            let body = "fixture-provider-private-key";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    let model = f
        .state
        .db
        .create_ai_model(nuono_core::db::CreateAiModel {
            name: "fixture",
            provider: "fixture",
            provider_type: "openai_compatible",
            model_id: "fixture",
            api_key: Some("fixture-request-private-key"),
            base_url: &format!("http://{address}/v1"),
            is_default: false,
        })
        .await
        .unwrap();
    let payload = json!({"model_id":model.id,"messages":[{"role":"user","content":"test"}]});
    let (status, body) = f.request("POST", "/api/ai/chat", payload.clone()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!body.to_string().contains("private-key"));
    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ai/chat/stream")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        to_bytes(response.into_body(), 16384),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("error"), "{text}");
    assert!(!text.contains("private-key"));
    upstream.abort();
    f.close().await;
}

#[test]
fn failure_construction_cannot_accidentally_produce_a_success_status() {
    for status in [
        StatusCode::OK,
        StatusCode::CREATED,
        StatusCode::NO_CONTENT,
        StatusCode::TEMPORARY_REDIRECT,
    ] {
        let response = ApiReply::<()>::error(status, "fixture-private-detail").into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
    let legacy = axum::Json(ApiResponse::<()> {
        success: false,
        data: None,
        error: Some("fixture-private-detail".into()),
    });
    assert_eq!(
        ApiReply::from(legacy).into_response().status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn closing_database_operations_rejects_backup_and_restore_routes_before_writing() {
    let f = Fixture::new(false).await;
    f.state.database_operations.close();
    let id = "00000000-0000-0000-0000-000000000000";
    for (path, body) in [
        ("/api/database/backup".to_string(), Value::Null),
        ("/api/database/transfers".into(), Value::Null),
        (
            "/api/database/restore".into(),
            json!({"filename":"nuono_manual_backup_fixture.dump"}),
        ),
        (
            format!("/api/database/transfers/{id}/restore"),
            json!({"confirmation":id}),
        ),
    ] {
        let (status, body) = f.request("POST", &path, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {body}");
        assert_eq!(body["success"], false);
        assert!(body["error"].as_str().unwrap().contains("服务正在关闭"));
    }
    assert!(!f.state.paths.data.join(crate::backup::BACKUP_DIR).exists());
    assert!(
        !f.state
            .paths
            .data
            .join(crate::backup::RESTORE_MARKER)
            .exists()
    );
    assert!(f.state.manager.framework_control.last_signal().is_none());
    f.close().await;
}

#[tokio::test]
async fn admin_platform_identities_enforce_owner_write_and_drive_chat_permissions() {
    let f = Fixture::new(true).await;
    let owner_token = f.session("site-owner", "owner").await;
    let admin_token = f.session("site-admin", "admin").await;
    let user_token = f.session("normal-user", "user").await;

    let owner = f
        .state
        .db
        .find_user_by_identifier("site-owner")
        .await
        .unwrap()
        .unwrap();
    let admin = f
        .state
        .db
        .find_user_by_identifier("site-admin")
        .await
        .unwrap()
        .unwrap();

    // 普通用户无法读取全站平台身份列表。
    let (status, _) = f.authenticated("/api/users/identities", &user_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 管理员可读列表，但不得替任意账号手动绑定身份（防止提权到 Owner）。
    let (status, body) = f.authenticated("/api/users/identities", &admin_token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"], json!([]));

    let (status, _) = f
        .authenticated_json(
            "POST",
            &format!("/api/users/{}/identities", owner.id),
            &admin_token,
            json!({"platform": "onebot", "native_user_id": "2609145334"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 所有者可直接将 OneBot QQ 号绑定到自己账号，以及把飞书身份绑定到管理员账号。
    let (status, bound_owner) = f
        .authenticated_json(
            "POST",
            &format!("/api/users/{}/identities", owner.id),
            &owner_token,
            json!({"platform": "onebot", "native_user_id": "2609145334"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{bound_owner}");
    let owner_identity_id = bound_owner["data"]["id"].as_str().unwrap().to_owned();
    let owner_identity_rev = bound_owner["data"]["revision"].as_str().unwrap().to_owned();

    let (status, bound_admin) = f
        .authenticated_json(
            "POST",
            &format!("/api/users/{}/identities", admin.id),
            &owner_token,
            json!({
                "platform": "feishu",
                "native_user_id": "ou_admin_01",
                "namespace": "cli_app_1"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{bound_admin}");

    // 官 Q 缺少命名空间、Telegram 非数字 ID、或重复绑定已归属身份均应拒绝。
    for (target, payload, expected) in [
        (
            owner.id,
            json!({"platform": "qq_official", "native_user_id": "USER_OPENID"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            owner.id,
            json!({"platform": "telegram", "native_user_id": "not-numeric"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            admin.id,
            json!({"platform": "onebot", "native_user_id": "2609145334"}),
            StatusCode::CONFLICT,
        ),
    ] {
        let (status, body) = f
            .authenticated_json(
                "POST",
                &format!("/api/users/{target}/identities"),
                &owner_token,
                payload,
            )
            .await;
        assert_eq!(status, expected, "{body}");
    }

    // 汇总列表按角色呈现生效聊天权限。
    let (status, list) = f.authenticated("/api/users/identities", &owner_token).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let rows = list["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["username"], "site-owner");
    assert_eq!(rows[0]["chat_permission"], "BotOwner");
    assert_eq!(rows[1]["username"], "site-admin");
    assert_eq!(rows[1]["chat_permission"], "BotAdmin");

    // 聊天端实时权限裁决立即生效。
    let perms = nuono_core::core::PermissionManager::new(f.state.db.clone());
    assert_eq!(
        perms
            .evaluate(&json!({"platform": "onebot", "user_id": 2609145334_i64}))
            .await,
        nuono_core::core::ActorPermission::Allowed(nuono_core::core::PermissionLevel::BotOwner)
    );
    assert_eq!(
        perms
            .evaluate(
                &json!({"platform": "feishu", "self_id": "cli_app_1", "user_id": "ou_admin_01"})
            )
            .await,
        nuono_core::core::ActorPermission::Allowed(nuono_core::core::PermissionLevel::BotAdmin)
    );

    // 管理员不得代为解绑；所有者解绑后聊天权限立即收回。
    let (status, _) = f
        .authenticated_json(
            "DELETE",
            &format!("/api/users/{}/identities/{owner_identity_id}", owner.id),
            &admin_token,
            json!({"revision": owner_identity_rev}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = f
        .authenticated_json(
            "DELETE",
            &format!("/api/users/{}/identities/{owner_identity_id}", owner.id),
            &owner_token,
            json!({"revision": owner_identity_rev}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        perms
            .evaluate(&json!({"platform": "onebot", "user_id": 2609145334_i64}))
            .await,
        nuono_core::core::ActorPermission::Allowed(nuono_core::core::PermissionLevel::Everyone)
    );

    f.close().await;
}

#[tokio::test]
async fn activity_statistics_are_staff_only_and_validate_query_ranges() {
    let f = Fixture::new(true).await;
    let owner = f.session("activity-owner", "owner").await;
    let user = f.session("activity-user", "user").await;
    assert_eq!(
        f.request("GET", "/api/statistics", json!(null)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.authenticated("/api/statistics", &user).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, body) = f.authenticated("/api/statistics?days=7", &owner).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["daily"].as_array().unwrap().len(), 7);
    assert_eq!(body["data"]["totals"]["messages"], "0");
    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/statistics")
                .header("authorization", format!("Bearer {owner}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        f.authenticated("/api/statistics?days=0", &owner).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.authenticated("/api/statistics?days=91", &owner).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.authenticated("/api/statistics?days=no", &owner).await.0,
        StatusCode::BAD_REQUEST
    );
    f.close().await;
}

#[tokio::test]
async fn message_trend_splits_received_and_sent_counts() {
    use sea_orm::{ActiveModelTrait, Set};

    let f = Fixture::new(true).await;
    let owner = f.session("trend-owner", "owner").await;
    let now = nuono_core::clock::db_now();
    let yesterday = now - chrono::Duration::hours(24);

    for (direction, role, ts) in [
        ("inbound", "user", now),
        ("inbound", "user", now),
        ("inbound", "user", now),
        ("outbound", "assistant", now),
        ("outbound", "assistant", now),
        ("inbound", "user", yesterday),
        ("outbound", "assistant", yesterday),
        ("outbound", "assistant", yesterday),
    ] {
        let ts_ms = ts.timestamp_millis();
        nuono_core::entity::messages::ActiveModel {
            platform: Set("onebot".into()),
            bot_instance_id: Set(Some("trend-bot".into())),
            native_sender_id: Set(Some("u1".into())),
            native_group_id: Set(Some("g1".into())),
            direction: Set(direction.into()),
            role: Set(role.into()),
            text_content: Set(String::new()),
            content_blocks_json: Set("[]".into()),
            occurred_at_ms: Set(ts_ms),
            created_at_ms: Set(ts_ms),
            ..Default::default()
        }
        .insert(f.state.db.orm())
        .await
        .unwrap();
    }

    let sum_items = |items: &[Value]| -> (i64, i64) {
        items.iter().fold((0_i64, 0_i64), |(r, s), item| {
            assert!(
                item.get("count").is_none(),
                "旧字段 count 不应继续出现在趋势接口中: {item}"
            );
            let received = item["received"].as_str().unwrap().parse::<i64>().unwrap();
            let sent = item["sent"].as_str().unwrap().parse::<i64>().unwrap();
            (r + received, s + sent)
        })
    };

    let (status, today_body) = f
        .authenticated("/api/trend?range=today&tz_offset=0", &owner)
        .await;
    assert_eq!(status, StatusCode::OK, "{today_body}");
    let today_items = today_body["data"].as_array().unwrap();
    assert!(!today_items.is_empty());
    assert_eq!(sum_items(today_items), (3, 2));

    let (status, yesterday_body) = f
        .authenticated("/api/trend?range=yesterday&tz_offset=0", &owner)
        .await;
    assert_eq!(status, StatusCode::OK, "{yesterday_body}");
    let yesterday_items = yesterday_body["data"].as_array().unwrap();
    assert_eq!(yesterday_items.len(), 24);
    assert_eq!(sum_items(yesterday_items), (1, 2));

    let (status, week_body) = f
        .authenticated("/api/trend?range=week&tz_offset=0", &owner)
        .await;
    assert_eq!(status, StatusCode::OK, "{week_body}");
    let week_items = week_body["data"].as_array().unwrap();
    assert_eq!(week_items.len(), 7);
    assert_eq!(sum_items(week_items), (4, 4));

    f.close().await;
}

#[tokio::test]
async fn core_commands_config_toggle_and_safety_floor_contract() {
    let f = Fixture::new(true).await;
    let owner = f.session("cmd-owner", "owner").await;
    let user = f.session("cmd-user", "user").await;

    // 权限边界：未登录 401，普通用户 403。
    assert_eq!(
        f.request("GET", "/api/commands/core/config", json!(null))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.authenticated("/api/commands/core/config", &user).await.0,
        StatusCode::FORBIDDEN
    );

    // 默认目录仅包含 help / status / restart，不包含已移除的语言切换指令。
    let (status, initial) = f.authenticated("/api/commands/core/config", &owner).await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    assert_eq!(initial["data"]["prefix"], "nuono");
    let ids: Vec<&str> = initial["data"]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["help", "status", "restart"]);

    // 尝试突破安全下限（将 restart 降级为 Everyone）、空前缀、重复别名均被 400 拒绝。
    for invalid_payload in [
        json!({
            "prefix": "nuono",
            "commands": [
                {"id": "restart", "enabled": true, "permission": "Everyone", "aliases": []}
            ]
        }),
        json!({
            "prefix": "   ",
            "commands": [
                {"id": "help", "enabled": true, "permission": "Everyone", "aliases": []}
            ]
        }),
        json!({
            "prefix": "my bot",
            "commands": [
                {"id": "help", "enabled": true, "permission": "Everyone", "aliases": []}
            ]
        }),
        json!({
            "prefix": "nuono",
            "commands": [
                {"id": "help", "enabled": true, "permission": "Everyone", "aliases": ["同义词"]},
                {"id": "status", "enabled": true, "permission": "Everyone", "aliases": ["同义词"]}
            ]
        }),
    ] {
        let (status, body) = f
            .authenticated_json("PUT", "/api/commands/core/config", &owner, invalid_payload)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    // 保存自定义前缀、快捷别名与权限升级。
    let (status, updated) = f
        .authenticated_json(
            "PUT",
            "/api/commands/core/config",
            &owner,
            json!({
                "prefix": "小助手",
                "commands": [
                    {"id": "help", "enabled": true, "permission": "Everyone", "aliases": ["菜单", "菜单"]},
                    {"id": "status", "enabled": true, "permission": "GroupAdmin", "aliases": ["/status"]},
                    {"id": "restart", "enabled": true, "permission": "BotAdmin", "aliases": []}
                ]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["data"]["prefix"], "小助手");
    let help_item = updated["data"]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "help")
        .unwrap();
    assert_eq!(help_item["aliases"], json!(["菜单"]));
    assert!(
        help_item["usage"]
            .as_str()
            .unwrap()
            .starts_with("小助手 帮助")
    );

    // 单条开关切换保留前缀、权限与别名。
    let (status, toggled) = f
        .authenticated_json(
            "POST",
            "/api/commands/restart/toggle",
            &owner,
            json!({"origin": "core", "enabled": false}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{toggled}");

    // 统一目录接口返回最新系统指令元数据。
    let (status, directory) = f.authenticated("/api/commands", &owner).await;
    assert_eq!(status, StatusCode::OK, "{directory}");
    let core_items: Vec<&Value> = directory["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["origin"] == "core")
        .collect();
    assert_eq!(core_items.len(), 3);
    assert_eq!(core_items[0]["name"], "help");
    assert_eq!(core_items[0]["aliases"], json!(["菜单"]));
    assert_eq!(core_items[1]["name"], "status");
    assert_eq!(core_items[1]["permission"], "GroupAdmin");
    assert_eq!(core_items[2]["name"], "restart");
    assert_eq!(core_items[2]["enabled"], false);
    assert_eq!(core_items[2]["min_permission"], "BotAdmin");

    // 运行时认领校验：普通闲聊和已删除的语言指令绝不被拦截，自定义前缀与别名正确命中。
    let core = &f.state.manager.core_commands;
    assert!(!core.claims_message("nuono今天怎么这么慢").await);
    assert!(!core.claims_message("nuono语言").await);
    assert!(!core.claims_message("nuono群语言").await);
    assert!(!core.claims_message("nuono帮助").await);
    assert!(core.claims_message("小助手帮助").await);
    assert!(core.claims_message("小助手 帮助").await);
    assert!(core.claims_message("/小助手 help").await);
    assert!(core.claims_message("菜单").await);
    assert!(core.claims_message("/status").await);

    f.close().await;
}
