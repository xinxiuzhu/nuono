//! 管理员专用的数据库连接地址配置接口（修改后于下次启动生效）。

use crate::api::input::ApiJson as Json;
use axum::{
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use nuono_core::{config::DatabaseConnectionSettings, error::NuonoError};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{AppState, api::ApiResponse};

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_database_settings, save_database_settings))
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::system_config::DatabaseConnectionSettings)]
pub(super) struct DatabaseSettingsResponse {
    url: String,
    active_url: String,
    revision: String,
    restart_required: bool,
}

impl From<DatabaseConnectionSettings> for DatabaseSettingsResponse {
    fn from(value: DatabaseConnectionSettings) -> Self {
        Self {
            url: value.url,
            active_url: value.active_url,
            revision: value.revision,
            restart_required: value.restart_required,
        }
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::system_config::SaveDatabaseConnection)]
pub(super) struct SaveDatabaseConnection {
    url: String,
    expected_revision: String,
}

fn reply(result: nuono_core::error::Result<DatabaseConnectionSettings>) -> Response {
    let mut response = match result {
        Ok(settings) => ApiResponse::ok(DatabaseSettingsResponse::from(settings)).into_response(),
        Err(NuonoError::Conflict(message)) => (
            StatusCode::CONFLICT,
            ApiResponse::<DatabaseSettingsResponse>::conflict(message),
        )
            .into_response(),
        Err(error) => ApiResponse::<DatabaseSettingsResponse>::from_err(&error).into_response(),
    };
    // 该专用管理接口按设计会返回包含完整凭据的数据库连接串，必须禁止缓存。
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

#[utoipa::path(
    get,
    path = "/database",
    operation_id = "system_config_get_database_settings",
    tag = "system_config",
    description = "管理员读取启动文件中的完整数据库连接地址。响应不可缓存，当前连接与文件配置分别返回。",
    responses((status = 200, description = "操作结果；请检查 success 与 error", body = ApiResponse<DatabaseSettingsResponse>))
)]
async fn get_database_settings(State(state): State<AppState>) -> Response {
    reply(state.config.database_settings().await)
}

#[utoipa::path(
    post,
    path = "/database",
    operation_id = "system_config_save_database_settings",
    tag = "system_config",
    description = "只修改启动文件中的 database.url。需要读取时的 revision，保存后重启生效，不迁移数据或切换当前连接。",
    request_body = SaveDatabaseConnection,
    responses(
        (status = 200, description = "操作结果；请检查 success 与 error", body = ApiResponse<DatabaseSettingsResponse>),
        (status = 409, description = "配置文件已被其他操作修改，请重新加载后保存", body = ApiResponse<DatabaseSettingsResponse>)
    )
)]
async fn save_database_settings(
    State(state): State<AppState>,
    Json(body): Json<SaveDatabaseConnection>,
) -> Response {
    let _restore = state.restore_lock.lock().await;
    if std::fs::symlink_metadata(state.paths.data.join(crate::backup::RESTORE_MARKER)).is_ok()
        || crate::backup::transfer::is_pending(&state.paths.data)
    {
        return reply(Err(NuonoError::Conflict(
            "已有恢复请求等待重启，请完成恢复后再修改数据库连接。".into(),
        )));
    }
    reply(
        state
            .config
            .save_database_settings(&body.url, &body.expected_revision)
            .await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn editor_returns_full_values_without_cache_and_conflicts_keep_their_status() {
        let url = "postgres://fixture:sample-password@127.0.0.1/fixture";
        let response = reply(Ok(DatabaseConnectionSettings {
            url: url.into(),
            active_url: "sqlite::memory:".into(),
            revision: "fixture-revision".into(),
            restart_required: true,
        }));
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["data"]["url"], url);
        assert_eq!(value["data"]["active_url"], "sqlite::memory:");
        assert_eq!(value["data"]["restart_required"], true);
        let conflict = reply(Err(NuonoError::Conflict("配置版本冲突".into())));
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        assert_eq!(conflict.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[test]
    fn save_payload_accepts_only_the_url_and_matching_revision() {
        let valid = serde_json::json!({"url":"sqlite::memory:","expected_revision":"r1"});
        assert!(serde_json::from_value::<SaveDatabaseConnection>(valid.clone()).is_ok());
        let mut path_attempt = valid;
        path_attempt["path"] = "different.toml".into();
        assert!(serde_json::from_value::<SaveDatabaseConnection>(path_attempt).is_err());
        assert!(
            serde_json::from_value::<SaveDatabaseConnection>(serde_json::json!({
                "url":"sqlite::memory:"
            }))
            .is_err()
        );
    }
}
