//! 站内通知接口。
//!
//! 全部端点只操作当前登录者自己的通知：`user_id` 来自会话身份，
//! 不接受按 id 的裸读写 —— 否则任何登录用户都能猜 id 读别人的收件箱。

use crate::api::input::{ApiPath as Path, ApiQuery as Query};
use axum::{extract::State, http::HeaderMap};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::ApiResponse;
use nuono_core::entity::notifications;

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::notifications::NotificationItem)]
pub struct NotificationItem {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub read_at: Option<String>,
    pub created_at: String,
}

impl From<notifications::Model> for NotificationItem {
    fn from(m: notifications::Model) -> Self {
        Self {
            id: m.id,
            kind: m.kind,
            title: m.title,
            body: m.body,
            link: m.link,
            read_at: m.read_at.map(nuono_core::clock::format_wire_ts),
            created_at: nuono_core::clock::format_wire_ts(m.created_at),
        }
    }
}

/// 解析当前会话对应的账号，失败时返回可直接下发的错误响应。
async fn own_account(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<nuono_core::entity::users::Model, crate::api::ApiReply<serde_json::Value>> {
    let token = crate::api::auth::extract_token_from_headers(headers)
        .ok_or_else(|| ApiResponse::unauthenticated("请先登录"))?;
    crate::api::auth::authenticated_user_checked(&state.db, &token)
        .await
        .map_err(|error| ApiResponse::from_err(&error))?
        .ok_or_else(|| ApiResponse::unauthenticated("登录状态已过期"))
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[schema(as = api::notifications::ListQuery)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    pub limit: Option<u64>,
}

#[utoipa::path(
    get,
    path = "/",
    operation_id = "notifications_list",
    tag = "notifications",
    params(ListQuery),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<NotificationItem>>))
)]
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> crate::api::ApiReply<Vec<NotificationItem>> {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(e) => return e.cast(),
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    match state.db.list_notifications(user.id, limit).await {
        Ok(rows) => ApiResponse::ok(rows.into_iter().map(NotificationItem::from).collect()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    post,
    path = "/{id}/read",
    operation_id = "notifications_mark_read",
    tag = "notifications",
    params(("id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<bool>))
)]
pub async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> crate::api::ApiReply<bool> {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(e) => return e.cast(),
    };
    match state.db.mark_notification_read(user.id, id).await {
        Ok(changed) => ApiResponse::ok(changed),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    post,
    path = "/read-all",
    operation_id = "notifications_mark_all_read",
    tag = "notifications",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<u64>))
)]
pub async fn mark_all_read(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> crate::api::ApiReply<u64> {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(e) => return e.cast(),
    };
    match state.db.mark_all_notifications_read(user.id).await {
        Ok(n) => ApiResponse::ok(n),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    delete,
    path = "/{id}",
    operation_id = "notifications_delete",
    tag = "notifications",
    params(("id" = String, Path)),
    responses((status = 200, description = "删除当前账号的一条通知", body = ApiResponse<bool>))
)]
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> crate::api::ApiReply<bool> {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(e) => return e.cast(),
    };
    match state.db.delete_notification(user.id, id).await {
        Ok(deleted) => ApiResponse::ok(deleted),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    delete,
    path = "/",
    operation_id = "notifications_delete_all",
    tag = "notifications",
    responses((status = 200, description = "清空当前账号的通知", body = ApiResponse<u64>))
)]
pub async fn delete_all(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> crate::api::ApiReply<u64> {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(e) => return e.cast(),
    };
    match state.db.delete_notifications_for_user(user.id).await {
        Ok(deleted) => ApiResponse::ok(deleted),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    get,
    path = "/unread-count",
    operation_id = "notifications_unread_count",
    tag = "notifications",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::AnyJson>))
)]
pub async fn unread_count(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> crate::api::ApiReply<serde_json::Value> {
    let user = match own_account(&state, &headers).await {
        Ok(user) => user,
        Err(e) => return e.cast(),
    };
    match state.db.unread_notification_count(user.id).await {
        Ok(n) => ApiResponse::ok(serde_json::json!({ "unread": n })),
        Err(e) => ApiResponse::from_err(&e),
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(mark_all_read))
        .routes(routes!(unread_count))
        .routes(routes!(mark_read))
        .routes(routes!(delete))
        .routes(routes!(delete_all))
}
