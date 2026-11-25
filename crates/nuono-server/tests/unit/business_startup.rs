use super::*;
use crate::api::contract_tests::Fixture;
use axum::{body::Body, http::Request};
use nuono_core::db::{NewUser, ROLE_OWNER, STATUS_ACTIVE};
use serde_json::json;
use std::sync::atomic::AtomicBool;
use tower::ServiceExt;

async fn owner(db: &Database) {
    db.create_first_owner(NewUser {
        username: "startup-owner".into(),
        email: None,
        password: "synthetic-setup-password-93".into(),
        role: ROLE_OWNER.into(),
        status: STATUS_ACTIVE.into(),
        email_verified: false,
    })
    .await
    .unwrap()
    .expect("an isolated empty database should accept its first owner");
}

async fn wait_phase(control: &BusinessStartup, expected: BusinessPhase) {
    let mut phases = control.phase.subscribe();
    tokio::time::timeout(Duration::from_secs(5), phases.wait_for(|p| *p == expected))
        .await
        .expect("business phase should advance")
        .expect("controller remains alive");
}

fn supervisor(fixture: &Fixture) -> (HostBackgroundTasks, oneshot::Sender<()>) {
    let mut tasks = HostBackgroundTasks::new();
    let (started, receiver) = oneshot::channel();
    register(
        &mut tasks,
        fixture.state.business_startup.clone(),
        receiver,
        fixture.state.manager.clone(),
        fixture.state.db.clone(),
        fixture.state.config.clone(),
        fixture.state.bot_instances.clone(),
    );
    tasks.seal();
    (tasks, started)
}

#[tokio::test]
async fn http_is_reported_only_after_serve_has_been_polled() {
    let (started, mut ready) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    let readiness = Arc::new(AtomicBool::new(false));
    let polled = Arc::new(AtomicBool::new(false));
    let observed = polled.clone();
    let future = serve_with_readiness(
        async move {
            observed.store(true, Ordering::Release);
            finished.await.unwrap();
            Ok(())
        },
        started,
        readiness.clone(),
        CancellationToken::new(),
    );
    tokio::pin!(future);
    assert!(!polled.load(Ordering::Acquire));
    assert!(!readiness.load(Ordering::Acquire));
    assert!(ready.try_recv().is_err());
    assert!(futures::poll!(&mut future).is_pending());
    assert!(polled.load(Ordering::Acquire));
    assert!(readiness.load(Ordering::Acquire));
    assert_eq!(ready.try_recv(), Ok(()));
    finish.send(()).unwrap();
    future.await.unwrap();
}

#[tokio::test]
async fn immediate_http_failure_never_releases_business() {
    let (started, ready) = oneshot::channel();
    let readiness = Arc::new(AtomicBool::new(false));
    let result = serve_with_readiness(
        async { Err(std::io::Error::other("synthetic serve failure")) },
        started,
        readiness.clone(),
        CancellationToken::new(),
    )
    .await;
    assert!(result.is_err());
    assert!(!readiness.load(Ordering::Acquire));
    assert!(ready.await.is_err());
}

#[tokio::test]
async fn owner_before_http_still_waits_for_http_then_starts_once() {
    let fixture = Fixture::prepared(false).await;
    owner(&fixture.state.db).await;
    let (tasks, started) = supervisor(&fixture);
    tokio::task::yield_now().await;
    assert!(!fixture.state.manager.business_is_running());
    assert_eq!(
        fixture.state.business_startup.phase(),
        BusinessPhase::WaitingHttp
    );
    started.send(()).unwrap();
    wait_phase(&fixture.state.business_startup, BusinessPhase::Running).await;
    assert!(fixture.state.manager.business_is_running());
    for _ in 0..8 {
        fixture.state.business_startup.setup_changed();
    }
    tokio::task::yield_now().await;
    assert_eq!(
        fixture.state.business_startup.phase(),
        BusinessPhase::Running
    );
    tasks.cancel();
    tasks.wait().await;
    fixture.close().await;
}

#[tokio::test]
async fn setup_api_releases_business_and_stays_one_shot() {
    let fixture = Fixture::prepared(true).await;
    let (tasks, started) = supervisor(&fixture);
    started.send(()).unwrap();
    wait_phase(&fixture.state.business_startup, BusinessPhase::WaitingSetup).await;
    assert!(!fixture.state.manager.business_is_running());
    let request = json!({"username":"startup-owner","password":"synthetic-setup-password-93"});
    let (status, response) = fixture.request("POST", "/api/setup", request.clone()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{response}");
    wait_phase(&fixture.state.business_startup, BusinessPhase::Running).await;
    let (status, _) = fixture.request("POST", "/api/setup", request).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(fixture.state.db.owner_count().await.unwrap(), 1);
    tasks.cancel();
    tasks.wait().await;
    fixture.close().await;
}

#[tokio::test]
async fn committed_owner_without_http_notification_is_recovered() {
    let fixture = Fixture::prepared(false).await;
    let (tasks, started) = supervisor(&fixture);
    started.send(()).unwrap();
    wait_phase(&fixture.state.business_startup, BusinessPhase::WaitingSetup).await;
    // 模拟数据库事务已提交，但客户端在收到响应前断开了 HTTP 连接的情况。
    owner(&fixture.state.db).await;
    wait_phase(&fixture.state.business_startup, BusinessPhase::Running).await;
    assert!(fixture.state.manager.business_is_running());
    tasks.cancel();
    tasks.wait().await;
    fixture.close().await;
}

#[tokio::test]
async fn shutdown_while_waiting_cannot_be_reopened_by_setup() {
    let fixture = Fixture::prepared(false).await;
    let (tasks, started) = supervisor(&fixture);
    started.send(()).unwrap();
    wait_phase(&fixture.state.business_startup, BusinessPhase::WaitingSetup).await;
    fixture.state.business_startup.close();
    fixture.state.manager.request_business_shutdown();
    tasks.cancel();
    owner(&fixture.state.db).await;
    fixture.state.business_startup.setup_changed();
    tasks.wait().await;
    assert_eq!(
        fixture.state.business_startup.phase(),
        BusinessPhase::Stopped
    );
    assert!(!fixture.state.manager.business_is_running());
    assert!(fixture.state.manager.start_business().await.is_err());
    fixture.close().await;
}

#[tokio::test]
async fn management_health_stays_available_while_business_ingress_is_closed() {
    let fixture = Fixture::prepared(false).await;
    let app = crate::http_server::router(fixture.state.clone());
    for (path, expected) in [
        ("/livez", axum::http::StatusCode::OK),
        ("/readyz", axum::http::StatusCode::OK),
        ("/api/setup/status", axum::http::StatusCode::OK),
        (
            "/api/plugin/unknown/business",
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "path={path}");
    }
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ingress/missing/instance")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    fixture.close().await;
}
