use std::{collections::HashMap, net::SocketAddr};

use axum::{
    extract::{ConnectInfo, Query, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use tracing::{debug, info, warn};

use crate::{AppState, api::auth};
use nuono_core::auth::Capability;

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

fn onebot_token<'a>(headers: &'a HeaderMap, query: &'a HashMap<String, String>) -> Option<&'a str> {
    bearer_token(headers)
        .or_else(|| query.get("access_token").map(String::as_str))
        .or_else(|| {
            headers
                .get("x-access-token")
                .and_then(|value| value.to_str().ok())
        })
}

/// handler 层鉴权判定：空 token = 放行。
///
/// OneBot WS token 是可选配置，与监听地址无关；配置后才要求客户端提供匹配 token。
fn onebot_token_is_authorized(configured: &str, provided: Option<&str>) -> bool {
    // 常数时间比较：逐字节短路比较会泄露前缀匹配长度。
    configured.is_empty()
        || provided.is_some_and(|token| {
            nuono_core::auth::constant_time_eq(token.as_bytes(), configured.as_bytes())
        })
}

/// OneBot WebSocket 入口。认证在升级连接之前完成。
pub async fn onebot_ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if let Some(response) = crate::business_http::reject_unavailable(&state) {
        return response;
    }
    let Some(listener) = state.bot_instances.source.listener_token("onebot") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cfg = state.config.snapshot().await;
    let provided = onebot_token(&headers, &query);
    let auth_enabled = !cfg.security.ws_token.is_empty();
    if !onebot_token_is_authorized(&cfg.security.ws_token, provided) {
        metrics::counter!("nuono_ws_auth_rejected_total", "kind" => "onebot").increment(1);
        warn!(client_ip = %addr.ip(), "拒绝未认证的 OneBot WebSocket 连接");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let role = headers
        .get("x-client-role")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let ip = addr.ip().to_string();
    let manager = state.manager.clone();
    // 记录当前 OneBot 网关连接所使用的 Host/Origin，使后续签发的临时媒体链接能生成该网关可访问的回源 URL
    // （OneBot 网关通常部署在另一台机器或独立容器中）。
    let base_url = nuono_core::core::media_links::base_from_headers(&headers);

    if auth_enabled {
        info!(client_ip = %ip, role = %role, "收到已认证的 OneBot WebSocket 连接");
    } else {
        info!(client_ip = %ip, role = %role, "收到 OneBot WebSocket 连接（未启用鉴权）");
    }
    ws.on_upgrade(move |socket| async move {
        tokio::select! { biased; _=listener.cancelled()=>{}, _=manager.handle_inbound_ws_with_base(adapt_socket(socket),ip,base_url)=>{} }
    })
}

/// 管理日志 WebSocket 入口。浏览器使用现有 session Cookie 完成认证。
///
/// 这里要求管理员角色：日志流是全局的，不按群或账号切分，普通用户读到的会是别人的消息内容
/// 和运行细节。该端点挂在 `/ws` 下而不是 `/api` 下，`security_middleware` 看不到它。
pub async fn log_ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !observable_session(&state, &headers).await {
        metrics::counter!("nuono_ws_auth_rejected_total", "kind" => "log_viewer").increment(1);
        // 已登录但不具备 `ViewObservability` 的客户打开日志页就会走到这里，是常规流量；
        // 记 warn 会让真正的异常被噪声淹没，所以与其它两处授权拒绝保持 info。
        info!(client_ip = %addr.ip(), "拒绝日志 WebSocket 连接：缺少可观测性能力或未登录");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let manager = state.manager.clone();
    ws.on_upgrade(move |socket| async move {
        handle_log_ws(socket, manager).await;
    })
}

async fn observable_session(state: &AppState, headers: &HeaderMap) -> bool {
    let config = state.config.snapshot().await;
    if !config.security.admin_auth_enabled {
        return true;
    }
    let token = auth::extract_token_from_headers(headers);
    match token.as_deref() {
        Some(token) => auth::authenticated_with(&state.db, token, Capability::ViewObservability)
            .await
            .is_some(),
        None => false,
    }
}

fn adapt_socket(
    socket: axum::extract::ws::WebSocket,
) -> impl futures::Stream<Item = Result<nuono_app::transport::TransportFrame, axum::Error>>
+ futures::Sink<nuono_app::transport::TransportFrame, Error = axum::Error>
+ Unpin
+ Send {
    use futures::{SinkExt, StreamExt};
    socket
        .with(|frame| futures::future::ready(Ok(into_web_frame(frame))))
        .map(|result| result.map(from_web_frame))
}

fn from_web_frame(frame: axum::extract::ws::Message) -> nuono_app::transport::TransportFrame {
    use axum::extract::ws::Message;
    use nuono_app::transport::{TransportClose, TransportFrame};
    match frame {
        Message::Text(value) => TransportFrame::Text(value.to_string()),
        Message::Binary(value) => TransportFrame::Binary(value.to_vec()),
        Message::Ping(value) => TransportFrame::Ping(value.to_vec()),
        Message::Pong(value) => TransportFrame::Pong(value.to_vec()),
        Message::Close(value) => TransportFrame::Close(value.map(|close| TransportClose {
            code: close.code,
            reason: close.reason.to_string(),
        })),
    }
}

fn into_web_frame(frame: nuono_app::transport::TransportFrame) -> axum::extract::ws::Message {
    use axum::extract::ws::{CloseFrame, Message};
    use nuono_app::transport::TransportFrame;
    match frame {
        TransportFrame::Text(value) => Message::Text(value.into()),
        TransportFrame::Binary(value) => Message::Binary(value.into()),
        TransportFrame::Ping(value) => Message::Ping(value.into()),
        TransportFrame::Pong(value) => Message::Pong(value.into()),
        TransportFrame::Close(value) => Message::Close(value.map(|close| CloseFrame {
            code: close.code,
            reason: close.reason.into(),
        })),
    }
}

pub async fn handle_log_ws(
    socket: axum::extract::ws::WebSocket,
    _manager: std::sync::Arc<nuono_core::core::ConnectionManager>,
) {
    use futures::{StreamExt, sink::SinkExt};

    debug!("[LogViewer] WebSocket 连接已建立");

    let (mut sender, mut receiver) = socket.split();
    let mut broadcast_rx = nuono_core::telemetry::log_broadcast_subscribe();

    // 先订阅实时日志广播、再读取最近 200 条缓存快照：快照序列化期间产生的新日志会留在广播队列中随后下发，
    // 前端日志页会按进程内递增的日志事件 ID 自动去重。
    let entries = nuono_core::telemetry::get_recent_logs(200);
    debug!(count = entries.len(), "发送日志历史快照");
    let batch = serde_json::to_string(&entries).unwrap_or_default();
    if batch.is_empty()
        || sender
            .send(axum::extract::ws::Message::Text(batch.into()))
            .await
            .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            inbound = receiver.next() => {
                if inbound.is_none() {
                    break;
                }
            }
            event = broadcast_rx.recv() => match event {
                Ok(log_entry) => {
                    let Ok(msg) = serde_json::to_string(&log_entry) else {
                        continue;
                    };
                    if sender
                        .send(axum::extract::ws::Message::Text(msg.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    info!(skipped, "日志 WebSocket 丢失事件，关闭连接以触发快照重连");
                    let _ = sender
                        .send(axum::extract::ws::Message::Close(None))
                        .await;
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    debug!("[LogViewer] WebSocket 连接已关闭");
}

#[cfg(test)]
mod tests {
    use super::{bearer_token, onebot_token, onebot_token_is_authorized};
    use axum::http::{HeaderMap, HeaderValue, header};
    use std::collections::HashMap;

    #[test]
    fn onebot_token_accepts_bearer_and_query() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer header-token"),
        );
        let query = HashMap::from([("access_token".to_string(), "query-token".to_string())]);

        assert_eq!(bearer_token(&headers), Some("header-token"));
        assert_eq!(onebot_token(&headers, &query), Some("header-token"));

        headers.remove(header::AUTHORIZATION);
        assert_eq!(onebot_token(&headers, &query), Some("query-token"));
    }

    #[test]
    fn onebot_token_auth_is_optional_when_not_configured() {
        assert!(onebot_token_is_authorized("", None));
        assert!(onebot_token_is_authorized("", Some("any-token")));
    }

    #[test]
    fn onebot_token_auth_requires_matching_token_when_configured() {
        assert!(onebot_token_is_authorized(
            "configured-token",
            Some("configured-token")
        ));
        assert!(!onebot_token_is_authorized("configured-token", None));
        assert!(!onebot_token_is_authorized(
            "configured-token",
            Some("wrong-token")
        ));
    }
}
