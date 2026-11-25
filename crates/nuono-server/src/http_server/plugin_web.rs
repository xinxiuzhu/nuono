//! 插件动态 Web 路由分发：仅在精确匹配到已启用插件的路由后，才按该路由声明的上限读取 HTTP 请求体。
use axum::{
    body::Body,
    extract::{Extension, Path, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{Html, IntoResponse, Response},
};
use futures::StreamExt;
use nuono_core::plugin::{PluginWebRequest, PluginWebResponse, PluginWebRoute};
use std::collections::HashMap;

pub(super) async fn handle(
    State(state): State<crate::AppState>,
    Path(full_path): Path<String>,
    caller: Option<Extension<crate::middleware::CallerIdentity>>,
    method: Method,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    request: Request,
) -> Response {
    if let Some(response) = crate::business_http::reject_unavailable(&state) {
        return response;
    }
    let (plugin_name, path) = match full_path.split_once('/') {
        Some((name, rest)) => (name, format!("/{rest}")),
        None => (full_path.as_str(), String::from("/")),
    };
    let host = &state.manager.plugin_host;
    let route = host.plugin_web_route(plugin_name, &method, &path).await;
    let body = match read_body(route.as_ref(), request.into_body()).await {
        Ok(body) => body,
        Err(status) => return status.into_response(),
    };
    let request = PluginWebRequest {
        method,
        path,
        query: query
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .collect::<Vec<_>>()
            .join("&"),
        headers,
        body,
        user: caller.map(|Extension(caller)| {
            let role = caller.role.to_string();
            nuono_core::plugin::PluginWebUser {
                user_id: caller.user_id,
                username: caller.username,
                role,
            }
        }),
    };
    response(host.handle_web_request(plugin_name, &request).await)
}

async fn read_body(route: Option<&PluginWebRoute>, body: Body) -> Result<Vec<u8>, StatusCode> {
    let route = route.ok_or(StatusCode::NOT_FOUND)?;
    if !(1..=32 * 1024 * 1024).contains(&route.max_body_bytes) {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        if chunk.len() > route.max_body_bytes.saturating_sub(bytes.len()) {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn response(value: PluginWebResponse) -> Response {
    match value {
        PluginWebResponse::Html(html) => Html(html).into_response(),
        PluginWebResponse::Json(value) => axum::Json(value).into_response(),
        PluginWebResponse::Raw { content_type, body } => {
            ([(axum::http::header::CONTENT_TYPE, content_type)], body).into_response()
        }
        PluginWebResponse::Http {
            status,
            headers,
            body,
        } => http_response(status, headers, body)
            .unwrap_or_else(|| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        PluginWebResponse::Status(code, message) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            message,
        )
            .into_response(),
        PluginWebResponse::Redirect(url) => {
            (StatusCode::FOUND, [(axum::http::header::LOCATION, url)]).into_response()
        }
        PluginWebResponse::NotFound => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn http_response(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Option<Response> {
    if !(200..=599).contains(&status)
        || (matches!(status, 204 | 205 | 304) && !body.is_empty())
        || headers.len() > 64
        || headers.iter().try_fold(0usize, |size, (name, value)| {
            size.checked_add(name.len())?.checked_add(value.len())
        })? > 16 * 1024
    {
        return None;
    }
    let mut parsed = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
        // 传输分帧与逐跳（hop-by-hop）响应头由底层 HTTP 服务器统一管理，不允许插件自定义返回。
        if matches!(
            name.as_str(),
            "connection"
                | "content-length"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) {
            return None;
        }
        parsed.append(name, HeaderValue::from_str(&value).ok()?);
    }
    Some((StatusCode::from_u16(status).ok()?, parsed, body).into_response())
}

#[cfg(test)]
#[path = "../../tests/unit/http_server/plugin_web/tests.rs"]
mod tests;
