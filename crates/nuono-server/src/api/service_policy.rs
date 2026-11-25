//! 部署级服务策略管理接口（即便某个必选策略插件加载失败，所有者仍可通过此接口修复配置以恢复服务）。
use crate::{
    AppState,
    api::{
        ApiReply, ApiResponse,
        input::{ApiCaller, ApiJson},
    },
};
use axum::extract::State;
use utoipa_axum::{router::OpenApiRouter, routes};
#[derive(serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct DeploymentPolicy {
    pub required_providers: Vec<String>,
    pub platform_roles_enabled: bool,
}
#[utoipa::path(get,path="/service-policy",tag="system",responses((status=200,body=ApiResponse<DeploymentPolicy>)))]
async fn get(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
) -> ApiReply<DeploymentPolicy> {
    if !caller.is_owner() {
        return ApiResponse::forbidden("只有部署所有者可管理服务策略");
    }
    ApiResponse::from_result(
        async {
            Ok(DeploymentPolicy {
                required_providers: state.db.required_service_providers().await?,
                platform_roles_enabled: state
                    .db
                    .get_global_config("configuration.platform_roles_enabled")
                    .await?
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        }
        .await,
    )
}
#[utoipa::path(post,path="/service-policy",tag="system",request_body=DeploymentPolicy,responses((status=200,body=ApiResponse<DeploymentPolicy>)))]
async fn set(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiJson(policy): ApiJson<DeploymentPolicy>,
) -> ApiReply<DeploymentPolicy> {
    if !caller.is_owner() {
        return ApiResponse::forbidden("只有部署所有者可管理服务策略");
    }
    ApiResponse::from_result(
        async {
            state
                .manager
                .plugin_host
                .update_deployment_policy(
                    caller.user_id,
                    &policy.required_providers,
                    policy.platform_roles_enabled,
                )
                .await?;
            Ok(policy)
        }
        .await,
    )
}
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get, set))
}
