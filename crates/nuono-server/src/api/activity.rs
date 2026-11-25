use crate::api::input::ApiQuery as Query;
use crate::{
    AppState,
    api::{ApiReply, ApiResponse},
};
use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, header},
};
use nuono_core::NuonoError;
use nuono_core::service::stats::{ActivityFilter, ActivityReport};

#[utoipa::path(get, path = "/api/statistics", operation_id = "statistics_activity", tag = "stats",
    params(("days" = Option<u32>, Query, description = "UTC 日历天数，1–90"),
        ("platform" = Option<String>, Query), ("scope_id" = Option<String>, Query),
        ("user_id" = Option<String>, Query), ("plugin" = Option<String>, Query),
        ("user_key" = Option<String>, Query), ("scope_key" = Option<String>, Query)),
    responses((status = 200, description = "消息活跃度与插件执行情况", body = ApiResponse<ActivityReport>)))]
pub async fn statistics(
    State(state): State<AppState>,
    Query(filter): Query<ActivityFilter>,
) -> (HeaderMap, ApiReply<ActivityReport>) {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        state.db.activity_report(filter),
    )
    .await
    {
        Ok(Ok(report)) => ApiResponse::ok(report),
        Ok(Err(error)) => ApiResponse::from_err(&error),
        Err(_) => ApiResponse::from_err(&NuonoError::Timeout("统计查询超时，请缩小范围".into())),
    };
    (headers, response)
}
