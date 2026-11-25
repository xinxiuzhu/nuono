use super::*;
use crate::api::contract_tests::Fixture;
use axum::body::to_bytes;
use serde_json::Value;

#[tokio::test]
async fn expected_waits_are_identifiable_and_retryable_without_being_cached() {
    for (phase, code, state) in [
        (
            BusinessPhase::WaitingHttp,
            "business_starting",
            "waiting_http",
        ),
        (
            BusinessPhase::WaitingSetup,
            "setup_required",
            "waiting_setup",
        ),
        (BusinessPhase::Starting, "business_starting", "starting"),
    ] {
        let response = unavailable_response(phase);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "5");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response
                .extensions()
                .get::<ExpectedBusinessWait>()
                .unwrap()
                .code(),
            code
        );
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], code);
        assert_eq!(body["business_state"], state);
        assert!(!body["message"].as_str().unwrap().is_empty());
    }
}

#[tokio::test]
async fn failure_and_shutdown_are_not_marked_as_expected_waits() {
    for (phase, code) in [
        (BusinessPhase::Failed, "business_start_failed"),
        (BusinessPhase::Stopped, "business_stopped"),
        (BusinessPhase::Running, "business_unavailable"),
    ] {
        let response = unavailable_response(phase);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            response
                .extensions()
                .get::<ExpectedBusinessWait>()
                .is_none()
        );
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], code);
        assert_ne!(body["code"], "setup_required");
    }
}

#[tokio::test]
async fn business_gate_opens_after_startup_and_remains_closed_after_shutdown() {
    let fixture = Fixture::prepared(false).await;
    assert!(reject_unavailable(&fixture.state).is_some());
    fixture.state.manager.start_business().await.unwrap();
    assert!(reject_unavailable(&fixture.state).is_none());
    fixture.state.manager.request_business_shutdown();
    fixture.state.business_startup.close();
    let response = reject_unavailable(&fixture.state).unwrap();
    assert!(
        response
            .extensions()
            .get::<ExpectedBusinessWait>()
            .is_none()
    );
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["code"], "business_stopped");
    fixture.close().await;
}
