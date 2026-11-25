//! AI 服务商真实调用量汇总接口（绝不暴露提示词正文、密钥凭据或未经实测的 Token 估算值）。
use crate::api::input::ApiQuery as Query;
use crate::{AppState, api::ApiResponse};
use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, header},
};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_usage))
}
#[derive(Deserialize, utoipa::IntoParams)]
struct UsageQuery {
    #[serde(default = "default_days")]
    days: u32,
}
fn default_days() -> u32 {
    30
}

#[utoipa::path(get,path="/usage",operation_id="ai_usage",tag="ai",params(UsageQuery),responses((status=200,body=ApiResponse<serde_json::Value>)))]
async fn get_usage(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> (HeaderMap, crate::api::ApiReply<serde_json::Value>) {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if !matches!(query.days, 7 | 30 | 90) {
        return (
            headers,
            ApiResponse::bad_request("统计天数只支持 7、30 或 90"),
        );
    }
    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        state.db.ai_usage_summary(query.days),
    )
    .await
    {
        Ok(Ok(summary)) => match serde_json::to_value(summary) {
            Ok(value) => ApiResponse::ok(value),
            Err(_) => ApiResponse::internal("AI 用量统计序列化失败"),
        },
        Ok(Err(nuono_core::NuonoError::Validation(message))) => ApiResponse::bad_request(message),
        Ok(Err(_)) => ApiResponse::internal("读取 AI 用量统计失败，请稍后重试"),
        Err(_) => ApiResponse::timeout("读取 AI 用量统计超时，请稍后重试"),
    };
    (headers, response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_window_is_thirty_days_and_query_requires_an_unsigned_integer() {
        assert_eq!(
            serde_json::from_value::<UsageQuery>(serde_json::json!({}))
                .unwrap()
                .days,
            30
        );
        assert!(serde_json::from_value::<UsageQuery>(serde_json::json!({"days":-1})).is_err());
        assert!(serde_json::from_value::<UsageQuery>(serde_json::json!({"days":7.5})).is_err());
    }
}
