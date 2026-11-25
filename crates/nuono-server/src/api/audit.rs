use crate::api::input::ApiQuery as Query;
use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::ApiResponse;

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[schema(as = api::audit::ListCommandLogsQuery)]
#[into_params(parameter_in = Query)]
pub struct ListCommandLogsQuery {
    pub limit: Option<u64>,
    /// 游标分页：上一页最后一条的 id。
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub before_id: Option<i64>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[schema(as = api::audit::CommandLogsPage)]
pub struct CommandLogsPage {
    pub items: Vec<nuono_core::audit::CommandLogRecord>,
    pub has_more: bool,
    /// 有更多数据时，下一页应传的 `before_id`。
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub next_before_id: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/command-logs",
    operation_id = "audit_list_command_logs",
    tag = "audit",
    params(ListCommandLogsQuery),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<CommandLogsPage>))
)]
pub async fn list_command_logs(
    State(state): State<AppState>,
    Query(query): Query<ListCommandLogsQuery>,
) -> crate::api::ApiReply<CommandLogsPage> {
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    match nuono_core::audit::get_command_logs_page(&state.db, limit, query.before_id).await {
        Ok((items, has_more)) => {
            let next_before_id = has_more.then(|| items.last().map(|r| r.id)).flatten();
            ApiResponse::ok(CommandLogsPage {
                items: items.into_iter().map(safe_command_log).collect(),
                has_more,
                next_before_id,
            })
        }
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    get,
    path = "/command-logs/export",
    operation_id = "audit_export_command_logs",
    tag = "audit",
    responses((status = 200, description = "CSV 导出", body = String, content_type = "text/csv"))
)]
pub async fn export_command_logs(State(state): State<AppState>) -> Response {
    let rows = match nuono_core::audit::get_command_logs_for_export(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            return (
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                format!("审计导出失败：{}", e.kind()),
            )
                .into_response();
        }
    };

    let mut csv = String::from("\u{feff}id,bot_id,user_id,command,timestamp\n");
    for row in rows.into_iter().map(safe_command_log) {
        csv.push_str(&csv_line(&[
            &row.id.to_string(),
            &row.bot_id,
            &row.user_id,
            &row.command,
            row.timestamp.as_deref().unwrap_or(""),
        ]));
    }

    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"command-logs.csv\"",
            ),
        ],
        csv,
    )
        .into_response()
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
#[schema(as = api::audit::ListEventLogsQuery)]
#[into_params(parameter_in = Query)]
pub struct ListEventLogsQuery {
    pub event_type: Option<String>,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub run_id: Option<i64>,
    pub limit: Option<u64>,
    /// 游标分页：上一页最后一条的 id。
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub before_id: Option<i64>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[schema(as = api::audit::EventLogsPage)]
pub struct EventLogsPage {
    pub items: Vec<nuono_core::audit::EventLogRecord>,
    pub has_more: bool,
    /// 有更多数据时，下一页应传的 `before_id`。
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub next_before_id: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/event-logs",
    operation_id = "audit_list_event_logs",
    tag = "audit",
    params(ListEventLogsQuery),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<EventLogsPage>))
)]
pub async fn list_event_logs(
    State(state): State<AppState>,
    Query(query): Query<ListEventLogsQuery>,
) -> crate::api::ApiReply<EventLogsPage> {
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    let filter = nuono_core::audit::EventLogFilter {
        event_type: query.event_type,
        run_id: query.run_id,
        before_id: query.before_id,
    };

    match nuono_core::audit::get_event_logs_page(&state.db, filter, limit).await {
        Ok((items, has_more)) => {
            let next_before_id = has_more.then(|| items.last().map(|r| r.id)).flatten();
            ApiResponse::ok(EventLogsPage {
                items: items.into_iter().map(safe_event_log).collect(),
                has_more,
                next_before_id,
            })
        }
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    get,
    path = "/event-logs/export",
    operation_id = "audit_export_event_logs",
    tag = "audit",
    params(ListEventLogsQuery),
    responses((status = 200, description = "CSV 导出", body = String, content_type = "text/csv"))
)]
pub async fn export_event_logs(
    State(state): State<AppState>,
    Query(query): Query<ListEventLogsQuery>,
) -> Response {
    let filter = nuono_core::audit::EventLogFilter {
        event_type: query.event_type,
        run_id: query.run_id,
        before_id: query.before_id,
    };

    let rows = match nuono_core::audit::get_event_logs_for_export(&state.db, filter).await {
        Ok(rows) => rows,
        Err(e) => {
            return (
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                format!("审计导出失败：{}", e.kind()),
            )
                .into_response();
        }
    };

    // BOM 让 Excel 正确识别 UTF-8；列名与 EventLogRecord 序列化字段一致。
    let mut csv = String::from("\u{feff}id,event_type,bot_id,data,timestamp,actor\n");
    for row in rows.into_iter().map(safe_event_log) {
        csv.push_str(&csv_line(&[
            &row.id.to_string(),
            &row.event_type,
            row.bot_id.as_deref().unwrap_or(""),
            row.data.as_deref().unwrap_or(""),
            row.timestamp.as_deref().unwrap_or(""),
            row.actor.as_deref().unwrap_or(""),
        ]));
    }

    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"event-logs.csv\"",
            ),
        ],
        csv,
    )
        .into_response()
}

fn safe_command_log(
    mut row: nuono_core::audit::CommandLogRecord,
) -> nuono_core::audit::CommandLogRecord {
    row.command = nuono_core::telemetry::sanitize_file_line(&row.command).0;
    row
}

fn safe_event_log(mut row: nuono_core::audit::EventLogRecord) -> nuono_core::audit::EventLogRecord {
    row.data = row
        .data
        .map(|data| nuono_core::telemetry::sanitize_file_line(&data).0);
    row
}

/// 最小 CSV 行编码：逗号、引号、换行都做转义，避免为此引入 CSV 库依赖。
fn csv_line(fields: &[&str]) -> String {
    let mut line = String::new();
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            line.push(',');
        }
        if field.contains([',', '"', '\n', '\r']) {
            line.push('"');
            line.push_str(&field.replace('"', "\"\""));
            line.push('"');
        } else {
            line.push_str(field);
        }
    }
    line.push('\n');
    line
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_command_logs))
        .routes(routes!(export_command_logs))
        .routes(routes!(list_event_logs))
        .routes(routes!(export_event_logs))
}

#[cfg(test)]
mod tests {
    use super::csv_line;

    #[test]
    fn csv_line_escapes_commas_quotes_and_newlines() {
        assert_eq!(csv_line(&["a", "b"]), "a,b\n");
        assert_eq!(csv_line(&["a,b", "c"]), "\"a,b\",c\n");
        assert_eq!(csv_line(&["say \"hi\"", "x"]), "\"say \"\"hi\"\"\",x\n");
        assert_eq!(csv_line(&["line1\nline2", "z"]), "\"line1\nline2\",z\n");
    }
}
