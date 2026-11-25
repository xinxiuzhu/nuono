use super::*;
use axum::{Router, body::Bytes, routing::post};
use nuono_core::plugin::PluginWebAccessPolicy;
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

#[tokio::test]
async fn exact_limit_is_accepted_and_streamed_overflow_stops_before_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatched = calls.clone();
    let app = Router::new().route(
        "/save",
        post(move |request: Request| {
            let calls = dispatched.clone();
            async move {
                let route =
                    PluginWebRoute::new(Method::POST, "/save", PluginWebAccessPolicy::Public)
                        .with_max_body_bytes(17 * 1024 * 1024);
                match read_body(Some(&route), request.into_body()).await {
                    Ok(bytes) => {
                        calls.fetch_add(1, Ordering::SeqCst);
                        bytes.len().to_string().into_response()
                    }
                    Err(status) => status.into_response(),
                }
            }
        }),
    );
    let at_limit = Body::from(vec![b' '; 17 * 1024 * 1024]);
    let result = app
        .clone()
        .oneshot(Request::post("/save").body(at_limit).unwrap())
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // 不带 Content-Length 头的分块流式请求：大小上限必须在逐块读取过程中实时生效，不能只依赖请求头声明的长度。
    let chunks = futures::stream::iter([
        Ok::<_, Infallible>(Bytes::from(vec![b' '; 17 * 1024 * 1024])),
        Ok(Bytes::from_static(b"x")),
    ]);
    let result = app
        .oneshot(
            Request::post("/save")
                .body(Body::from_stream(chunks))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn default_limit_remains_two_mib_and_missing_routes_do_not_poll_body() {
    let route = PluginWebRoute::new(Method::POST, "/save", PluginWebAccessPolicy::Public);
    assert_eq!(
        read_body(Some(&route), Body::from(vec![0; 2 * 1024 * 1024 + 1]))
            .await
            .unwrap_err(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let unread =
        futures::stream::poll_fn(|_| -> std::task::Poll<Option<Result<Bytes, Infallible>>> {
            panic!("a missing or disabled route must not read its body")
        });
    assert_eq!(
        read_body(None, Body::from_stream(unread))
            .await
            .unwrap_err(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn download_status_headers_and_exact_bytes_survive_http_response() {
    let result = response(PluginWebResponse::Http {
        status: 201,
        headers: vec![
            (
                "content-type".into(),
                "application/json; charset=utf-8".into(),
            ),
            (
                "content-disposition".into(),
                "attachment; filename=records.json".into(),
            ),
            ("cache-control".into(), "no-store".into()),
            ("referrer-policy".into(), "no-referrer".into()),
        ],
        body: br#"{"list":[]}"#.to_vec(),
    });
    assert_eq!(result.status(), StatusCode::CREATED);
    assert_eq!(
        result.headers()["content-disposition"],
        "attachment; filename=records.json"
    );
    assert_eq!(result.headers()["cache-control"], "no-store");
    assert_eq!(result.headers()["referrer-policy"], "no-referrer");
    assert_eq!(
        &axum::body::to_bytes(result.into_body(), 1024)
            .await
            .unwrap()[..],
        br#"{"list":[]}"#
    );
}

#[test]
fn invalid_headers_status_and_transport_framing_fail_closed() {
    for headers in [
        vec![("x-invalid\r\nheader".into(), "value".into())],
        vec![("x-header".into(), "value\r\ninjected: true".into())],
        vec![("Content-Length".into(), "999".into())],
        vec![("Transfer-Encoding".into(), "chunked".into())],
        vec![("Connection".into(), "x-header".into())],
    ] {
        assert_eq!(
            response(PluginWebResponse::Http {
                status: 200,
                headers,
                body: vec![]
            })
            .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    for status in [0, 101, 600, 999] {
        assert_eq!(
            response(PluginWebResponse::Http {
                status,
                headers: vec![],
                body: vec![]
            })
            .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
