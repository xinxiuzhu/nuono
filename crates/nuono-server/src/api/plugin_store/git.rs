//! 已安装源码插件的 Git 提交历史分页查询与版本切换 HTTP 接口。
use super::{ApiResponse, AppState, Json, OpenApiRouter, Path, State, context, routes};
use crate::api::input::ApiQuery as Query;
use crate::source_operations::{git, jobs};
use git::{GitHistory, HistoryQuery};
use serde::Deserialize;

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct CheckoutBody {
    commit: String,
    expected_commit: String,
}

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(history))
        .routes(routes!(history_by_name))
        .routes(routes!(refresh_git))
        .routes(routes!(checkout))
}

#[utoipa::path(get, path="/store/git/{id}", operation_id="plugin_git_history", tag="plugin_store", params(("id"=String,Path),("reference"=Option<String>,Query),("offset"=Option<usize>,Query)), responses((status=200,body=ApiResponse<GitHistory>)))]
async fn history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> crate::api::ApiReply<GitHistory> {
    match git::history_view(&context(&state), &id, &query).await {
        Ok(view) => ApiResponse::ok(view),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
#[utoipa::path(get, path="/store/git/by-name/{name}", operation_id="plugin_git_history_by_name", tag="plugin_store", params(("name"=String,Path),("reference"=Option<String>,Query),("offset"=Option<usize>,Query)), responses((status=200,body=ApiResponse<GitHistory>)))]
async fn history_by_name(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> crate::api::ApiReply<GitHistory> {
    match git::history_by_name(&context(&state), &name, &query).await {
        Ok(view) => ApiResponse::ok(view),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
#[utoipa::path(post,path="/store/git/{id}/refresh",operation_id="plugin_git_refresh",tag="plugin_store",params(("id"=String,Path)),responses((status=200,body=ApiResponse<String>)))]
async fn refresh_git(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> crate::api::ApiReply<String> {
    let result = git::refresh_git(&context(&state), &id).await;
    match result {
        Ok(message) => ApiResponse::ok(message),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
#[utoipa::path(post,path="/store/git/{id}/checkout",operation_id="plugin_git_checkout",tag="plugin_store",request_body=CheckoutBody,params(("id"=String,Path)),responses((status=200,body=ApiResponse<crate::source_schemas::StoreJobSchema>)))]
async fn checkout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CheckoutBody>,
) -> crate::api::ApiReply<jobs::StoreJob> {
    match jobs::submit_checkout(context(&state), id, body.commit, body.expected_commit).await {
        Ok(job) => ApiResponse::ok(job),
        Err(error) => ApiResponse::from_err(&error.0),
    }
}
