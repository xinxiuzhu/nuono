//! 平台适配器 HTTP Webhook 入站入口：HTTP 层负责业务开放检查、请求体大小限制与实例路由分发，具体平台签名与协议校验由各适配器内部完成。
use crate::AppState;
use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
async fn dispatch(
    state: AppState,
    adapter: &str,
    id: &str,
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = crate::business_http::reject_unavailable(&state) {
        return response;
    }
    if body.len() > 1024 * 1024 {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let request = nuono_adapter::AdapterRequest {
        method: method.to_string(),
        path: uri.path().into(),
        query: uri.query().unwrap_or_default().into(),
        headers: headers
            .iter()
            .filter_map(|(key, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (key.to_string(), value.into()))
            })
            .collect(),
        body: body.to_vec(),
    };
    match state
        .bot_instances
        .source
        .handle_request(adapter, id, request)
        .await
    {
        Ok(response) => {
            let Ok(status) = StatusCode::from_u16(response.status) else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            let mut output = (status, response.body).into_response();
            for (key, value) in response.headers {
                if let (Ok(key), Ok(value)) = (
                    key.parse::<axum::http::HeaderName>(),
                    value.parse::<axum::http::HeaderValue>(),
                ) {
                    output.headers_mut().append(key, value);
                }
            }
            output
        }
        Err(nuono_adapter::NuonoError::NotFound(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(nuono_adapter::NuonoError::Unauthorized(_)) => StatusCode::UNAUTHORIZED.into_response(),
        Err(
            nuono_adapter::NuonoError::Validation(_) | nuono_adapter::NuonoError::Serialization(_),
        ) => StatusCode::BAD_REQUEST.into_response(),
        Err(error) => {
            tracing::warn!(
                adapter,
                instance = id,
                kind = error.kind(),
                "适配器回调未接纳"
            );
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}
pub async fn handle(
    State(state): State<AppState>,
    Path((adapter, id)): Path<(String, String)>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(state, &adapter, &id, method, uri, headers, body).await
}
pub async fn legacy_feishu(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(state, "feishu", "feishu", method, uri, headers, body).await
}
pub async fn legacy_qq(
    State(state): State<AppState>,
    Path(id): Path<String>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(state, "qq_official", &id, method, uri, headers, body).await
}
