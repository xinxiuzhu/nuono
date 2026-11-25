use super::*;
use nuono_core::plugin::{PluginWebAsset, PluginWebPage};

fn view(generation: u64) -> PluginWebPageView {
    PluginWebPageView {
        plugin: "fixture".into(),
        generation,
        page: Arc::new(PluginWebPage::new(
            "overview",
            "Overview",
            vec![PluginWebAsset::new(
                "index.html",
                "text/html",
                b"<html></html>",
            )],
        )),
    }
}

#[tokio::test]
async fn asset_grants_are_scoped_and_reused_only_for_the_same_session_and_activation() {
    let sessions = PageSessions::new();
    let first = sessions
        .issue("session-a".into(), &view(1), Role::Owner)
        .await
        .unwrap();
    assert_eq!(
        first,
        sessions
            .issue("session-a".into(), &view(1), Role::Owner)
            .await
            .unwrap()
    );
    assert_ne!(
        first,
        sessions
            .issue("session-b".into(), &view(1), Role::Owner)
            .await
            .unwrap()
    );
    assert_ne!(
        first,
        sessions
            .issue("session-a".into(), &view(2), Role::Owner)
            .await
            .unwrap()
    );
    let user_ticket = sessions
        .issue("session-a".into(), &view(1), Role::User)
        .await
        .unwrap();
    assert_ne!(
        first, user_ticket,
        "user view never reuses a management asset grant"
    );
    assert_eq!(
        sessions.resolve(&user_ticket).await.unwrap().role,
        Role::User
    );
    let grant = sessions.resolve(&first).await.unwrap();
    assert_eq!(
        (grant.plugin.as_str(), grant.page.as_str(), grant.generation),
        ("fixture", "overview", 1)
    );
    assert!(sessions.resolve("../index.html").await.is_none());
    sessions.0.lock().await.get_mut(&first).unwrap().expires = Instant::now();
    assert!(sessions.resolve(&first).await.is_none());
}

struct PageFixture;
impl nuono_plugin::Plugin for PageFixture {
    fn name(&self) -> &str {
        "WebPageFixture"
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "Generic WebUI contract fixture"
    }
    fn web_pages(&self) -> Vec<PluginWebPage> {
        vec![
            PluginWebPage::new(
                "overview",
                "Overview",
                vec![PluginWebAsset::new(
                    "index.html",
                    "text/html",
                    b"<html><head></head><body>fixture</body></html>",
                )],
            )
            .with_access(nuono_plugin::PluginWebAccessPolicy::Authenticated),
            PluginWebPage::new(
                "staff",
                "Staff",
                vec![PluginWebAsset::new(
                    "index.html",
                    "text/html",
                    b"<html>staff only</html>",
                )],
            ),
        ]
    }
}

#[test]
fn http_pages_respect_roles_sessions_and_lifecycle() {
    const CHILD: &str = "NUONO_WEB_PAGE_CONTRACT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        nuono_core::plugin::source_loader::register_factories(vec![|| Box::new(PageFixture)])
            .unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for admin_auth_enabled in [true, false] {
                http_contract(admin_auth_enabled).await;
            }
        });
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "api::plugin_pages::tests::http_pages_respect_roles_sessions_and_lifecycle",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .expect("run isolated page factory contract");
    assert!(
        output.status.success(),
        "isolated page contract failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn http_contract(admin_auth_enabled: bool) {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;
    let fixture = super::super::contract_tests::Fixture::new(admin_auth_enabled).await;
    let state = fixture.state.clone();
    let user = state
        .db
        .create_user(nuono_core::db::NewUser {
            username: "page-customer".into(),
            password: "fixture-password".into(),
            role: "user".into(),
            email: None,
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap();
    let session = "page-contract-session";
    let key = format!(
        "auth_session_{}",
        hex::encode(Sha256::digest(session.as_bytes()))
    );
    assert!(
        state
            .db
            .create_session_for_verified_user(
                &user,
                &key,
                chrono::Utc::now() + chrono::Duration::hours(1)
            )
            .await
            .unwrap()
    );
    let app = super::super::router()
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::security_middleware,
        ))
        .with_state(state.clone());
    let owner_session = fixture.session("page-owner", "owner").await;
    for (label, token, expected_pages) in
        [("user", session, 1), ("owner", owner_session.as_str(), 2)]
    {
        let req = Request::builder()
            .uri("/api/plugin-pages")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(
            body["data"]["pages"].as_array().unwrap().len(),
            expected_pages,
            "catalog must honor {label} role even with admin_auth_enabled={admin_auth_enabled}"
        );
    }
    let restricted = Request::builder()
        .uri("/api/plugin-pages/WebPageFixture/staff")
        .header("authorization", format!("Bearer {session}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(restricted).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let request = |path: &str, auth: bool| {
        let mut request = Request::builder().uri(path);
        if auth {
            request = request.header("authorization", format!("Bearer {session}"));
        }
        request.body(Body::empty()).unwrap()
    };
    for path in [
        "/api/plugin-pages",
        "/api/plugin-pages/WebPageFixture/overview",
        "/api/plugin-pages/assets/overview",
    ] {
        for method in [axum::http::Method::GET, axum::http::Method::HEAD] {
            let mut anonymous = request(path, false);
            *anonymous.method_mut() = method;
            assert_eq!(
                app.clone().oneshot(anonymous).await.unwrap().status(),
                StatusCode::UNAUTHORIZED,
                "anonymous {path} must require an actual identity"
            );
        }
    }
    let response = app
        .clone()
        .oneshot(request("/api/plugin-pages", true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let data: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(
        data["data"]["pages"].as_array().unwrap().len(),
        1,
        "customer must never discover staff pages"
    );
    let initial_revision = data["data"]["pages"][0]["revision"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!initial_revision.is_empty());
    assert_eq!(
        app.clone()
            .oneshot(request("/api/plugin-pages/WebPageFixture/staff", true))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = app
        .clone()
        .oneshot(request("/api/plugin-pages/WebPageFixture/overview", true))
        .await
        .unwrap();
    let data: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    let content = data["data"]["content_path"].as_str().unwrap().to_string();
    let response = app.clone().oneshot(request(&content, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-frame-options"], "SAMEORIGIN");
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("/api/plugin-pages/bridge.js"));
    assert_eq!(
        app.clone()
            .oneshot(request(
                &content.replace("index.html", "private.html"),
                false
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    state
        .manager
        .plugin_host
        .toggle_plugin("WebPageFixture")
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(request(&content, false))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    state
        .manager
        .plugin_host
        .toggle_plugin("WebPageFixture")
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(request(&content, false))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND,
        "re-enabling must not revive a previous activation's grant"
    );
    let response = app
        .clone()
        .oneshot(request("/api/plugin-pages/WebPageFixture/overview", true))
        .await
        .unwrap();
    let data: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    let renewed = data["data"]["content_path"].as_str().unwrap();
    assert_ne!(content, renewed);
    assert_ne!(
        data["data"]["page"]["revision"].as_str().unwrap(),
        initial_revision
    );
    use sea_orm::ConnectionTrait;
    let expired_session = state
        .db
        .orm()
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            state.db.backend(),
            "UPDATE nuono_sessions SET created_at_ms = $1, expires_at_ms = $2 WHERE token_hash = $3",
            [
                946684799999_i64.into(),
                946684800000_i64.into(),
                Sha256::digest(session.as_bytes()).to_vec().into(),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(expired_session.rows_affected(), 1);
    assert_eq!(
        app.oneshot(request(renewed, false)).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    drop(state);
    fixture.close().await;
}
