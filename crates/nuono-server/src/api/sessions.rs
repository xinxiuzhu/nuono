use crate::api::input::ApiPath as Path;
use axum::extract::State;
use nuono_core::api::waiter::{WaitManager, WaitSnapshot};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::ApiResponse;

/// 会话信息
#[derive(Debug, Serialize, ToSchema)]
#[schema(as = api::sessions::SessionInfo)]
pub struct SessionInfo {
    /// 会话 ID
    pub session_id: String,
    /// 会话类型 (single/multiply)
    pub session_type: String,
    /// 机器人ID
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub bot_id: i64,
    /// 用户 ID
    pub user_id: String,
    /// 群组 ID (如果是群聊)
    pub group_id: Option<String>,
    /// 创建时间
    pub created_at: String,
}

/// 会话统计
#[derive(Debug, Serialize, ToSchema)]
#[schema(as = api::sessions::SessionStats)]
pub struct SessionStats {
    /// 单轮会话数量
    pub single_count: usize,
    /// 多轮会话数量
    pub multiply_count: usize,
    /// 总计
    pub total: usize,
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_sessions))
        .routes(routes!(get_session_stats))
        .routes(routes!(delete_session))
}

/// 获取会话列表
#[utoipa::path(
    get,
    path = "/",
    operation_id = "sessions_list_sessions",
    tag = "sessions",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<SessionInfo>>))
)]
async fn list_sessions(State(state): State<AppState>) -> crate::api::ApiReply<Vec<SessionInfo>> {
    ApiResponse::ok(session_rows(&state.manager.wait_manager))
}

fn session_rows(manager: &WaitManager) -> Vec<SessionInfo> {
    manager
        .list()
        .into_iter()
        .map(|wait| SessionInfo {
            session_id: wait.session_id,
            session_type: if wait.scope.multiply {
                "multiply"
            } else {
                "single"
            }
            .into(),
            bot_id: wait.scope.bot_id,
            user_id: wait.scope.user_id,
            group_id: wait.scope.group_id,
            created_at: nuono_core::clock::format_wire_ts(wait.created_at),
        })
        .collect()
}

/// 获取会话统计
#[utoipa::path(
    get,
    path = "/stats",
    operation_id = "sessions_get_session_stats",
    tag = "sessions",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<SessionStats>))
)]
async fn get_session_stats(State(state): State<AppState>) -> crate::api::ApiReply<SessionStats> {
    ApiResponse::ok(session_stats(&state.manager.wait_manager.list()))
}

fn session_stats(waits: &[WaitSnapshot]) -> SessionStats {
    let multiply_count = waits.iter().filter(|wait| wait.scope.multiply).count();
    SessionStats {
        single_count: waits.len() - multiply_count,
        multiply_count,
        total: waits.len(),
    }
}

/// 删除会话
#[utoipa::path(
    delete,
    path = "/{session_id}",
    operation_id = "sessions_delete_session",
    tag = "sessions",
    params(("session_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
async fn delete_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> crate::api::ApiReply<String> {
    cancel_session(&state.manager.wait_manager, &session_id);
    ApiResponse::ok(format!("会话 {} 已删除", session_id))
}

fn cancel_session(waits: &WaitManager, session_id: &str) {
    // 等待者被唤醒后，其 RAII guard 只清理自己的 SDK 实例代次。
    // 这里不按 session_id 删除旧实例索引，避免误删取消之后刚建立的新会话。
    waits.cancel(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_core::api::waiter::WaitScope;
    use std::time::Duration;

    #[tokio::test]
    async fn session_list_stats_and_delete_use_live_waits() {
        let waits = WaitManager::new();
        let first = waits.register_scoped_wait(
            "single".into(),
            Duration::from_secs(30),
            WaitScope {
                bot_id: 42,
                user_id: "opaque:user".into(),
                group_id: Some("group:one".into()),
                ..Default::default()
            },
        );
        let second = waits.register_scoped_event_wait(
            "multi".into(),
            Duration::from_secs(30),
            WaitScope {
                bot_id: 43,
                user_id: "other".into(),
                multiply: true,
                ..Default::default()
            },
        );
        let rows = session_rows(&waits);
        assert_eq!(rows.len(), 2);
        let single = rows.iter().find(|row| row.session_id == "single").unwrap();
        assert_eq!(single.bot_id, 42);
        assert_eq!(single.user_id, "opaque:user");
        assert_eq!(single.group_id.as_deref(), Some("group:one"));
        assert_eq!(single.session_type, "single");
        assert!(single.created_at.parse::<i64>().is_ok());
        let stats = session_stats(&waits.list());
        assert_eq!(
            (stats.single_count, stats.multiply_count, stats.total),
            (1, 1, 2)
        );
        cancel_session(&waits, "single");
        assert!(first.await.is_err(), "删除应唤醒实际等待者");
        assert!(!waits.respond("single", serde_json::json!({"text":"normal message"})));
        assert_eq!(session_rows(&waits).len(), 1);
        assert_eq!(session_stats(&waits.list()).multiply_count, 1);
        drop(second);
        assert!(session_rows(&waits).is_empty());
        assert_eq!(session_stats(&waits.list()).total, 0);
    }

    #[tokio::test]
    async fn expired_or_cancelled_waits_do_not_appear_as_active_sessions() {
        let waits = WaitManager::new();
        let expired = waits.register_wait("expired".into(), Duration::ZERO);
        let abandoned = waits.register_wait("abandoned".into(), Duration::from_secs(30));
        drop(abandoned);
        assert!(session_rows(&waits).is_empty());
        assert!(expired.await.is_err());
        assert_eq!(session_stats(&waits.list()).total, 0);
        cancel_session(&waits, "missing");
    }
}
