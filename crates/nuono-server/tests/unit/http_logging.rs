use super::*;
use crate::{api::contract_tests::Fixture, business_http};
use axum::{body::Body, http::header};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex, atomic::Ordering},
};
use tower::ServiceExt;
use tracing::{Dispatch, Level, instrument::WithSubscriber};

#[derive(Clone, Default)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl LogWriter {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn subscriber(&self, level: Level) -> Dispatch {
        Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_env_filter(format!("off,nuono::http={level}"))
                .with_ansi(false)
                .without_time()
                .finish(),
        )
    }
}

#[tokio::test]
async fn repeated_business_waits_stay_quiet_but_unmarked_503_is_an_error() {
    let fixture = Fixture::prepared(false).await;
    let app = crate::http_server::router(fixture.state.clone());
    let log = LogWriter::default();
    let subscriber = log.subscriber(Level::INFO);
    for _ in 0..3 {
        for (method, path) in [
            (
                "GET",
                "/api/plugin/missing/probe?access_token=private-test-token",
            ),
            ("POST", "/ingress/missing/instance"),
            ("POST", "/webhook/feishu"),
            ("POST", "/webhook/qq/missing"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header(header::ACCEPT_ENCODING, "gzip")
                        .body(Body::empty())
                        .unwrap(),
                )
                .with_subscriber(subscriber.clone())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(response.headers()[header::RETRY_AFTER], "5");
            assert!(
                response
                    .extensions()
                    .get::<ExpectedBusinessWait>()
                    .is_some()
            );
        }
    }
    for path in ["/readyz", "/livez", "/api/setup/status"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .with_subscriber(subscriber.clone())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    assert!(log.contents().is_empty(), "{}", log.contents());

    fixture.state.readiness.store(false, Ordering::Release);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let events = log.contents();
    assert_eq!(events.lines().count(), 1, "{events}");
    assert!(
        events.contains("ERROR") && events.contains("HTTP 请求失败"),
        "{events}"
    );
    assert!(
        events.contains("GET") && events.contains("/readyz") && events.contains("503"),
        "{events}"
    );
    fixture.close().await;
}

#[tokio::test]
async fn debug_wait_log_has_method_path_reason_and_no_query_credentials() {
    let fixture = Fixture::prepared(false).await;
    let app = crate::http_server::router(fixture.state.clone());
    let log = LogWriter::default();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ingress/missing/instance?access_token=private-query-secret")
                .header(header::AUTHORIZATION, "Bearer private-header-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .with_subscriber(log.subscriber(Level::DEBUG))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let events = log.contents();
    assert_eq!(events.lines().count(), 1, "{events}");
    for required in [
        "DEBUG",
        "POST",
        "/ingress/",
        "business_starting",
        "业务入口暂未开放",
    ] {
        assert!(events.contains(required), "missing {required}: {events}");
    }
    for forbidden in [
        "ERROR",
        "access_token",
        "private-query-secret",
        "private-header-secret",
    ] {
        assert!(
            !events.contains(forbidden),
            "unexpected {forbidden}: {events}"
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn headers_and_text_cannot_hide_failures_and_wait_marker_only_applies_to_503() {
    let request = Request::builder()
        .uri("/ws?access_token=private-test-token")
        .body(())
        .unwrap();
    let fake_wait = Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::RETRY_AFTER, "5")
        .header("x-business-state", "setup_required")
        .body("setup_required")
        .unwrap();
    assert!(matches!(
        HttpRequests
            .make_classifier(&request)
            .classify_response(&fake_wait),
        ClassifiedResponse::Ready(Err(_))
    ));

    let fixture = Fixture::prepared(false).await;
    let mut response = business_http::reject_unavailable(&fixture.state).unwrap();
    assert!(matches!(
        HttpRequests
            .make_classifier(&request)
            .classify_response(&response),
        ClassifiedResponse::Ready(Ok(()))
    ));
    for status in [StatusCode::INTERNAL_SERVER_ERROR, StatusCode::BAD_GATEWAY] {
        *response.status_mut() = status;
        assert!(matches!(
            HttpRequests
                .make_classifier(&request)
                .classify_response(&response),
            ClassifiedResponse::Ready(Err(_))
        ));
    }
    fixture.close().await;
}

#[test]
fn transport_failures_keep_request_context_without_media_tokens() {
    let request = Request::builder()
        .uri("/media/private-signed-token?token=private-query-token")
        .body(())
        .unwrap();
    let error = HttpRequests
        .make_classifier(&request)
        .classify_error(&io::Error::other("synthetic transport error"));
    let log = LogWriter::default();
    tracing::dispatcher::with_default(&log.subscriber(Level::INFO), || {
        log_failure(error, Duration::from_millis(3), &Span::none());
    });
    let events = log.contents();
    for required in [
        "ERROR",
        "GET",
        "/media/{token}",
        "synthetic transport error",
    ] {
        assert!(events.contains(required), "missing {required}: {events}");
    }
    assert!(!events.contains("private-"), "{events}");
}
