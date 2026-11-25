use crate::api::input::ApiJson as Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::ApiResponse;

const LOG_LEVEL_PRESETS: &[&str] = &["error", "warn", "info", "debug", "trace"];

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(files))
        .routes(routes!(file_content))
        .routes(routes!(get_settings, update_settings))
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[schema(as = api::logs::LogFileQuery)]
#[into_params(parameter_in = Query)]
pub struct LogFileQuery {
    /// 仅接受 YYYY-MM-DD.log，不接受路径。
    name: String,
    /// 从该字节位置向前读取；缺省时从当前文件末尾开始。
    before: Option<u64>,
    /// 默认 200 行，范围 1–500。
    limit: Option<usize>,
    /// 脱敏后的文本关键字；最多 256 字节，区分大小写。
    q: Option<String>,
    /// 精确匹配显式插件归属，最多 256 字节；留空表示全部。
    plugin: Option<String>,
    /// 定位指定行的字节起点并读取前后上下文；不能与 before、q 或 plugin 同用。
    around: Option<u64>,
}

#[utoipa::path(
    get, path = "/files", operation_id = "logs_files", tag = "logs",
    responses((status = 200, description = "文件输出状态及可读历史文件；即使关闭文件输出也可查看历史", body = ApiResponse<nuono_core::service::log_files::LogFileList>))
)]
async fn files() -> crate::api::ApiReply<nuono_core::service::log_files::LogFileList> {
    match nuono_core::service::log_files::list_files().await {
        Ok(files) => ApiResponse::ok(files),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[utoipa::path(
    get, path = "/files/content", operation_id = "logs_file_content", tag = "logs",
    params(LogFileQuery),
    responses((status = 200, description = "有界分页，内容已脱敏；搜索最多扫描约 1 MiB，上下文定位每侧最多约 1 MiB；空命中页仍需根据 next_before 继续搜索", body = ApiResponse<nuono_core::service::log_files::LogFilePage>))
)]
async fn file_content(
    crate::api::input::ApiQuery(query): crate::api::input::ApiQuery<LogFileQuery>,
) -> crate::api::ApiReply<nuono_core::service::log_files::LogFilePage> {
    match nuono_core::service::log_files::read_content(
        query.name,
        query.before,
        query.limit.unwrap_or(200),
        query.q,
        query.around,
        query.plugin,
    )
    .await
    {
        Ok(page) => ApiResponse::ok(page),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[derive(Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[schema(as = api::logs::LogQuery)]
#[into_params(parameter_in = Query)]
pub struct LogQuery {
    limit: Option<u64>,
}

#[utoipa::path(
    get,
    path = "/",
    operation_id = "logs_list",
    tag = "logs",
    params(LogQuery),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<nuono_core::service::logs::LogRecord>>))
)]
pub async fn list(
    State(state): State<AppState>,
    crate::api::input::ApiQuery(query): crate::api::input::ApiQuery<LogQuery>,
) -> crate::api::ApiReply<Vec<nuono_core::service::logs::LogRecord>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    match nuono_core::service::logs::get_logs(&state.db, limit).await {
        Ok(logs) => ApiResponse::ok(logs),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::logs::LogSettings)]
pub struct LogSettings {
    filter: String,
    preset: Option<String>,
    levels: &'static [&'static str],
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::logs::UpdateLogSettings)]
pub struct UpdateLogSettings {
    filter: String,
}

fn preset_for(filter: &str) -> Option<String> {
    let trimmed = filter.trim().to_ascii_lowercase();
    LOG_LEVEL_PRESETS
        .iter()
        .find(|level| trimmed == **level)
        .map(|level| (*level).to_string())
}

#[utoipa::path(
    get,
    path = "/settings",
    operation_id = "logs_get_settings",
    tag = "logs",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<LogSettings>))
)]
async fn get_settings() -> crate::api::ApiReply<LogSettings> {
    let filter = nuono_core::telemetry::current_filter();
    ApiResponse::ok(LogSettings {
        preset: preset_for(&filter),
        filter,
        levels: LOG_LEVEL_PRESETS,
    })
}

#[utoipa::path(
    patch,
    path = "/settings",
    operation_id = "logs_update_settings",
    tag = "logs",
    request_body = UpdateLogSettings,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<LogSettings>))
)]
async fn update_settings(
    State(state): State<AppState>,
    Json(body): Json<UpdateLogSettings>,
) -> crate::api::ApiReply<LogSettings> {
    let requested = body.filter.trim();
    if requested.is_empty() {
        return ApiResponse::bad_request("日志采集等级不能为空");
    }
    if let Err(error) = nuono_core::telemetry::validate_filter(requested) {
        return ApiResponse::bad_request(error);
    }

    let _update_guard = nuono_core::telemetry::acquire_filter_update_lock().await;
    let old_filter = nuono_core::telemetry::current_filter();
    if let Err(error) = nuono_core::telemetry::reload_filter(requested) {
        return ApiResponse::internal(error);
    }

    // 显式补丁：只落库 log.level，请求未给出的字段不参与；候选配置在事务内校验，
    // 落库失败时回滚数据库并把运行期过滤器一并还原。
    let patch = nuono_core::db::RuntimeConfigPatch {
        settings: serde_json::json!({ "log": { "level": requested } }),
        secrets: vec![],
    };
    if let Err(error) = state.config.update_patch(patch).await {
        let _ = nuono_core::telemetry::reload_filter(&old_filter);
        return ApiResponse::from_err(&error);
    }

    ApiResponse::ok(LogSettings {
        filter: requested.to_string(),
        preset: preset_for(requested),
        levels: LOG_LEVEL_PRESETS,
    })
}

#[cfg(test)]
mod tests {
    use super::preset_for;

    #[test]
    fn recognizes_only_plain_level_presets() {
        assert_eq!(preset_for("INFO").as_deref(), Some("info"));
        assert_eq!(preset_for("debug").as_deref(), Some("debug"));
        assert_eq!(preset_for("info,nuono_core=debug"), None);
    }
}
