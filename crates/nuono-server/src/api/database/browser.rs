//! 数据库只读浏览与数据导出的 HTTP 接口定义。
use super::policies::PluginPolicies;
use crate::api::input::{ApiPath as Path, ApiQuery as Query};
use crate::{AppState, api::ApiResponse};
use axum::{
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use nuono_core::db::{
    BrowseFilter, BrowsePage, BrowseRequest, MAX_EXPORT_LIMIT, TableInspection, TableSchema,
};
use nuono_core::error::NuonoError;
use serde::Deserialize;
use std::{future::Future, time::Duration};
use utoipa_axum::{router::OpenApiRouter, routes};

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(catalog))
        .routes(routes!(schema))
        .routes(routes!(rows))
        .routes(routes!(export))
}

#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct BrowseQuery {
    /// 从 0 开始的分页偏移量（上限 1,000,000）。
    offset: Option<u64>,
    /// 每页返回条数（1–200）；导出接口始终导出所有匹配记录（上限 5,000 条）。
    limit: Option<u64>,
    sort: Option<String>,
    descending: Option<bool>,
    /// 以 AND 组合的筛选条件 JSON 数组，最多 8 项，元素结构为 `{column, operator, value?, value_to?}`。
    /// 值统一传字符串；支持的比较符（contains/eq/is_null/is_not_null/between/gte/lte）取决于列类型。
    filters: Option<String>,
    filter_column: Option<String>,
    /// 按指定列做区分大小写的子串匹配（最长 2,048 字节）。
    filter_value: Option<String>,
    /// 导出格式：`csv` 或 `json`（仅 `/export` 接口使用）。
    format: Option<String>,
}

impl BrowseQuery {
    fn request(self) -> Result<BrowseRequest, NuonoError> {
        if self.filters.is_some() && (self.filter_column.is_some() || self.filter_value.is_some()) {
            return Err(NuonoError::InvalidInput(
                "filters 不能与旧筛选参数同时使用".into(),
            ));
        }
        let filters: Vec<BrowseFilter> = match self.filters {
            Some(json) if json.len() <= 210_000 => serde_json::from_str(&json).map_err(|_| {
                NuonoError::InvalidInput("filters 必须为有效的筛选条件 JSON 数组".into())
            })?,
            Some(_) => return Err(NuonoError::InvalidInput("筛选条件超过大小限制".into())),
            None => vec![],
        };
        if filters.len() > 8 {
            return Err(NuonoError::InvalidInput("最多组合 8 项筛选条件".into()));
        }
        Ok(BrowseRequest {
            filters,
            offset: self.offset.unwrap_or(0),
            limit: self.limit.unwrap_or(50),
            sort: self.sort,
            descending: self.descending.unwrap_or(false),
            filter_column: self.filter_column,
            filter_value: self.filter_value,
        })
    }
}

#[utoipa::path(get, path = "/catalog", operation_id = "database_catalog", tag = "database",
    responses((status = 200, description = "实际表与浏览策略", body = ApiResponse<Vec<TableInspection>>), (status = 504, description = "查询超时")))]
async fn catalog(State(state): State<AppState>) -> Response {
    match bounded(async {
        let policies = load_policies(&state).await?;
        let mut tables = state
            .db
            .inspect_tables_with_policies(&policies.tables)
            .await?;
        policies.assign_owners(&mut tables);
        Ok(tables)
    })
    .await
    {
        Ok(tables) => (
            [(header::CACHE_CONTROL, "no-store")],
            ApiResponse::ok(tables),
        )
            .into_response(),
        Err(error) => export_error(&error),
    }
}

#[utoipa::path(get, path = "/tables/{table}/schema", operation_id = "database_schema", tag = "database",
    params(("table" = String, Path, description = "精确的目录表名")),
    responses((status = 200, description = "列信息；readable=false 的列不会出现在记录或导出中", body = ApiResponse<TableSchema>), (status = 504, description = "查询超时")))]
async fn schema(State(state): State<AppState>, Path(table): Path<String>) -> Response {
    match bounded(async {
        let policies = load_policies(&state).await?;
        let mut schema = state
            .db
            .inspect_table_with_policy(&table, policies.policy(&table))
            .await?;
        policies.assign_owners(std::slice::from_mut(&mut schema.table));
        Ok(schema)
    })
    .await
    {
        Ok(schema) => (
            [(header::CACHE_CONTROL, "no-store")],
            ApiResponse::ok(schema),
        )
            .into_response(),
        Err(error) => export_error(&error),
    }
}

#[utoipa::path(get, path = "/tables/{table}/rows", operation_id = "database_rows", tag = "database",
    params(("table" = String, Path, description = "精确的目录表名"), BrowseQuery),
    responses((status = 200, description = "有界分页；单元格值为字符串或 null，保留整数精度", body = ApiResponse<BrowsePage>), (status = 504, description = "查询超时")))]
async fn rows(
    State(state): State<AppState>,
    Path(table): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Response {
    let request = match query.request() {
        Ok(request) => request,
        Err(error) => return export_error(&error),
    };
    match bounded(async {
        let policies = load_policies(&state).await?;
        state
            .db
            .browse_table_with_policy(&table, &request, policies.policy(&table))
            .await
    })
    .await
    {
        Ok(page) => ([(header::CACHE_CONTROL, "no-store")], ApiResponse::ok(page)).into_response(),
        Err(error) => export_error(&error),
    }
}

#[utoipa::path(get, path = "/tables/{table}/export", operation_id = "database_export", tag = "database",
    params(("table" = String, Path, description = "精确的目录表名"), BrowseQuery),
    responses(
        (status = 200, description = "全部过滤后的记录，最多 5,000 条；JSON 含 columns、rows 与 total；CSV 会防护电子表格公式单元格", content((String = "text/csv"), (BrowsePage = "application/json"))),
        (status = 400, description = "不支持的格式或导出过大", body = ApiResponse<String>),
        (status = 504, description = "查询超时")
    ))]
async fn export(
    State(state): State<AppState>,
    Path(table): Path<String>,
    Query(query): Query<BrowseQuery>,
) -> Response {
    let format = query.format.as_deref().unwrap_or("csv").to_string();
    if !matches!(format.as_str(), "csv" | "json") {
        return (
            StatusCode::BAD_REQUEST,
            ApiResponse::<String>::bad_request("导出格式仅支持 csv 或 json"),
        )
            .into_response();
    }
    let mut request = match query.request() {
        Ok(request) => request,
        Err(error) => return export_error(&error),
    };
    request.offset = 0;
    request.limit = MAX_EXPORT_LIMIT;
    let page = match bounded(async {
        let policies = load_policies(&state).await?;
        state
            .db
            .export_table_with_policy(&table, &request, policies.policy(&table))
            .await
    })
    .await
    {
        Ok(page) => page,
        Err(error) => return export_error(&error),
    };
    let (content_type, body) = if format == "csv" {
        ("text/csv; charset=utf-8", render_csv(&page))
    } else {
        match serde_json::to_string_pretty(&page) {
            Ok(body) => ("application/json; charset=utf-8", body),
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ApiResponse::<String>::internal(error),
                )
                    .into_response();
            }
        }
    };
    // 过滤非字母数字下划线字符，防止未经清洗的数据库表名直接拼入 HTTP 响应头。
    let safe_name: String = table
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{safe_name}.{format}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        body,
    )
        .into_response()
}

async fn load_policies(state: &AppState) -> Result<PluginPolicies, NuonoError> {
    PluginPolicies::load(state)
        .await
        .map_err(|_| NuonoError::DatabaseQuery("读取插件管理声明失败，请稍后重试".into()))
}

async fn bounded<T>(future: impl Future<Output = Result<T, NuonoError>>) -> Result<T, NuonoError> {
    bounded_with(Duration::from_secs(15), future).await
}

async fn bounded_with<T>(
    duration: Duration,
    future: impl Future<Output = Result<T, NuonoError>>,
) -> Result<T, NuonoError> {
    tokio::time::timeout(duration, future)
        .await
        .map_err(|_| NuonoError::Timeout("数据库查询超时，请缩小筛选范围或稍后重试".into()))?
}

fn export_error(error: &nuono_core::error::NuonoError) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        ApiResponse::<String>::from_err(error),
    )
        .into_response()
}

fn csv_cell(value: &str) -> String {
    // 仅加双引号不足以阻止 Excel 等表格软件执行公式注入，需对公式前缀字符额外加单引号转义。
    let formula = value.trim_start().starts_with(['=', '+', '-', '@'])
        || value.starts_with(['\t', '\r', '\n']);
    format!(
        "\"{}{}\"",
        if formula { "'" } else { "" },
        value.replace('"', "\"\"")
    )
}

fn render_csv(page: &BrowsePage) -> String {
    let mut csv = String::from("\u{feff}");
    csv.push_str(
        &page
            .columns
            .iter()
            .map(|c| csv_cell(c))
            .collect::<Vec<_>>()
            .join(","),
    );
    csv.push_str("\r\n");
    for row in &page.rows {
        csv.push_str(
            &page
                .columns
                .iter()
                .map(|column| {
                    csv_cell(
                        row.get(column)
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
        );
        csv.push_str("\r\n");
    }
    csv
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_parses_typed_filters_and_rejects_mixed_or_malformed_requests() {
        let filters = r#"[{"column":"id","operator":"eq","value":"9007199254740993"}]"#;
        let request = BrowseQuery {
            filters: Some(filters.into()),
            ..Default::default()
        }
        .request()
        .unwrap();
        assert_eq!(
            request.filters[0].value.as_deref(),
            Some("9007199254740993")
        );
        for query in [
            BrowseQuery {
                filters: Some("[]".into()),
                filter_column: Some("id".into()),
                filter_value: Some("1".into()),
                ..Default::default()
            },
            BrowseQuery {
                filters: Some("{}".into()),
                ..Default::default()
            },
            BrowseQuery {
                filters: Some(r#"[{"column":"id","operator":"eq","value":1}]"#.into()),
                ..Default::default()
            },
            BrowseQuery {
                filters: Some(
                    serde_json::to_string(&vec![
                        serde_json::json!({"column":"id", "operator":"eq", "value":"1"});
                        9
                    ])
                    .unwrap(),
                ),
                ..Default::default()
            },
        ] {
            let error = query.request().unwrap_err();
            assert_eq!(export_error(&error).status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn bounded_reads_time_out_and_allow_the_next_request() {
        let error = bounded_with::<()>(Duration::from_millis(1), std::future::pending())
            .await
            .unwrap_err();
        assert!(matches!(error, NuonoError::Timeout(_)));
        assert_eq!(export_error(&error).status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(bounded(async { Ok(42) }).await.unwrap(), 42);
    }
    #[test]
    fn csv_escapes_quotes_lines_and_formulas() {
        assert_eq!(csv_cell("a,\"b\"\nc"), "\"a,\"\"b\"\"\nc\"");
        assert_eq!(csv_cell("  =SUM(A1)"), "\"'  =SUM(A1)\"");
        assert_eq!(csv_cell("\tvalue"), "\"'\tvalue\"");
        assert_eq!(csv_cell("普通文本"), "\"普通文本\"");
    }

    #[test]
    fn csv_keeps_projection_order_and_omits_unselected_fields() {
        let page = BrowsePage {
            table: "users".into(),
            columns: vec!["username".into(), "id".into()],
            rows: vec![
                serde_json::json!({"id":"9007199254740993", "username":null, "password":"hidden"}),
            ],
            total: 1,
            offset: 0,
            limit: 50,
        };
        assert_eq!(
            render_csv(&page),
            "\u{feff}\"username\",\"id\"\r\n\"\",\"9007199254740993\"\r\n"
        );
    }

    #[test]
    fn openapi_registers_browser_paths_and_filter_parameters() {
        let doc = serde_json::to_value(router().into_openapi()).unwrap();
        for path in [
            "/catalog",
            "/tables/{table}/schema",
            "/tables/{table}/rows",
            "/tables/{table}/export",
        ] {
            assert!(doc["paths"][path]["get"].is_object(), "{path}");
        }
        let parameters = doc["paths"]["/tables/{table}/rows"]["get"]["parameters"]
            .as_array()
            .unwrap();
        for name in [
            "table",
            "offset",
            "limit",
            "sort",
            "descending",
            "filters",
            "filter_column",
            "filter_value",
        ] {
            assert!(
                parameters.iter().any(|parameter| parameter["name"] == name),
                "{name}"
            );
        }
        assert!(doc["components"]["schemas"]["BrowsePage"].is_object());
    }

    #[test]
    fn export_errors_cannot_be_mistaken_for_downloads() {
        use nuono_core::error::NuonoError;
        for (error, status) in [
            (
                NuonoError::InvalidInput("过大".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                NuonoError::PermissionDenied("restricted".into()),
                StatusCode::FORBIDDEN,
            ),
            (NuonoError::NotFound("table".into()), StatusCode::NOT_FOUND),
        ] {
            assert_eq!(export_error(&error).status(), status);
        }
    }
}
