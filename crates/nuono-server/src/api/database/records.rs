//! 需认证并记入审计日志的数据表记录增删改接口（客户端重试时可复用同一持久化操作 UUID 实现幂等）。
use crate::api::input::{ApiJson as Json, ApiPath as Path};
use crate::{AppState, api::ApiResponse, backup::RESTORE_MARKER, middleware::CallerIdentity};
use axum::{
    Extension,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use nuono_core::{
    db::{MutationOperation, MutationRequest, MutationResult},
    error::NuonoError,
};
use utoipa_axum::{router::OpenApiRouter, routes};

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create, update, delete))
}

#[utoipa::path(post, path = "/tables/{table}/records", operation_id = "database_create_record", tag = "database",
    params(("table" = String, Path)), request_body = MutationRequest,
    responses((status = 200, body = ApiResponse<MutationResult>), (status = 400, description = "字段无效"), (status = 409, description = "操作 UUID 冲突"), (status = 403, description = "受保护表或权限不足"), (status = 404, description = "表不存在"), (status = 503, description = "操作结果未确认；请使用同一 UUID 重试")))]
async fn create(
    State(state): State<AppState>,
    Path(table): Path<String>,
    caller: Option<Extension<CallerIdentity>>,
    Json(request): Json<MutationRequest>,
) -> Response {
    mutate(state, table, caller, request, MutationOperation::Create).await
}

#[utoipa::path(patch, path = "/tables/{table}/records", operation_id = "database_update_record", tag = "database",
    params(("table" = String, Path)), request_body = MutationRequest,
    responses((status = 200, body = ApiResponse<MutationResult>), (status = 400, description = "字段无效"), (status = 409, description = "记录已变更或已删除"), (status = 403, description = "受保护表或权限不足"), (status = 404, description = "表不存在"), (status = 503, description = "操作结果未确认；请使用同一 UUID 重试")))]
async fn update(
    State(state): State<AppState>,
    Path(table): Path<String>,
    caller: Option<Extension<CallerIdentity>>,
    Json(request): Json<MutationRequest>,
) -> Response {
    mutate(state, table, caller, request, MutationOperation::Update).await
}

#[utoipa::path(delete, path = "/tables/{table}/records", operation_id = "database_delete_record", tag = "database",
    params(("table" = String, Path)), request_body = MutationRequest,
    responses((status = 200, body = ApiResponse<MutationResult>), (status = 400, description = "缺少确认"), (status = 409, description = "记录已变更或已删除"), (status = 403, description = "受保护表或权限不足"), (status = 404, description = "表不存在"), (status = 503, description = "操作结果未确认；请使用同一 UUID 重试")))]
async fn delete(
    State(state): State<AppState>,
    Path(table): Path<String>,
    caller: Option<Extension<CallerIdentity>>,
    Json(request): Json<MutationRequest>,
) -> Response {
    mutate(state, table, caller, request, MutationOperation::Delete).await
}

async fn mutate(
    state: AppState,
    table: String,
    caller: Option<Extension<CallerIdentity>>,
    request: MutationRequest,
    operation: MutationOperation,
) -> Response {
    let actor = caller
        .map(|Extension(identity)| identity.username)
        .unwrap_or_else(|| "local-admin".into());
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let _guard = state.restore_lock.lock().await;
        if state.paths.data.join(RESTORE_MARKER).exists()
            || crate::backup::transfer::is_pending(&state.paths.data)
        {
            return Err(NuonoError::Conflict(
                "数据库已安排恢复，请在重启完成后重新读取记录".into(),
            ));
        }
        let policies = super::policies::PluginPolicies::load(&state)
            .await
            .map_err(|_| NuonoError::DatabaseQuery("读取插件管理声明失败，请稍后重试".into()))?;
        state
            .db
            .mutate_table_with_policy(&table, operation, &request, &actor, policies.policy(&table))
            .await
    })
    .await;
    match result {
        Ok(Ok(result)) => ApiResponse::ok(result).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(_) => error_response(NuonoError::Timeout(
            "操作结果尚未确认，请刷新数据，重试时保留原操作编号".into(),
        )),
    }
}

fn error_response(error: NuonoError) -> Response {
    let status = match error {
        NuonoError::InvalidInput(_) | NuonoError::Validation(_) => StatusCode::BAD_REQUEST,
        NuonoError::PermissionDenied(_) | NuonoError::Unauthorized(_) => StatusCode::FORBIDDEN,
        NuonoError::NotFound(_) => StatusCode::NOT_FOUND,
        NuonoError::Conflict(_) => StatusCode::CONFLICT,
        NuonoError::Timeout(_) | NuonoError::DatabaseConnection(_) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, ApiResponse::<MutationResult>::from_err(&error)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mutation_paths_and_conflicts_are_machine_readable() {
        let document = serde_json::to_value(router().into_openapi()).unwrap();
        for method in ["post", "patch", "delete"] {
            assert!(
                document["paths"]["/tables/{table}/records"][method]["requestBody"].is_object()
            );
            assert!(
                document["paths"]["/tables/{table}/records"][method]["responses"]["409"]
                    .is_object()
            );
        }
        assert_eq!(
            error_response(NuonoError::Conflict("changed".into())).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            error_response(NuonoError::PermissionDenied("protected".into())).status(),
            StatusCode::FORBIDDEN
        );
    }
}
