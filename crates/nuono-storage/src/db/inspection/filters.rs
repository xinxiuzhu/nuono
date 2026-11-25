//! 数据库巡检浏览器的有界强类型过滤条件构建器（仅允许对白名单可读列做筛选）。
use super::{
    BrowseRequest, ColumnInspection, MAX_FILTER_BYTES, identifier, invalid, parameter,
    readable_columns,
};
use crate::error::Result;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};
use sea_orm::{DbBackend, Value as SqlValue};
use serde::{Deserialize, Serialize};

pub const MAX_FILTERS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilterOperator {
    Contains,
    Eq,
    IsNull,
    IsNotNull,
    Between,
    Gte,
    Lte,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowseFilter {
    pub column: String,
    pub operator: FilterOperator,
    pub value: Option<String>,
    pub value_to: Option<String>,
}

pub(super) fn column_kind(
    table: &str,
    name: &str,
    physical: &str,
    edit: Option<&str>,
    backend: DbBackend,
) -> String {
    let physical = physical.to_ascii_lowercase();
    let base = physical.split('(').next().unwrap_or(&physical).trim();
    // SQLite 核心建表迁移把部分已知布尔列定义为 INTEGER、把部分历史 UTC 时间戳列定义为 TEXT；
    // 这里仅对已审核的核心表可读列做类型还原映射。
    if backend == DbBackend::Sqlite && readable_columns(table).contains(&name) {
        if base == "integer"
            && matches!(
                name,
                "enabled"
                    | "is_default"
                    | "customer_toggleable"
                    | "email_verified"
                    | "totp_enabled"
            )
        {
            return "boolean".into();
        }
        if base == "text"
            && matches!(
                name,
                "created_at"
                    | "updated_at"
                    | "last_login_at"
                    | "expire_at"
                    | "last_activity"
                    | "timestamp"
                    | "last_run_at"
                    | "read_at"
            )
        {
            return "datetime".into();
        }
    }
    if base == "boolean"
        || base == "bool"
        || (backend == DbBackend::Sqlite && base == "integer" && edit == Some("boolean"))
    {
        return "boolean".into();
    }
    if base == "date" {
        return "date".into();
    }
    if base.starts_with("timestamp") || base == "datetime" {
        return "datetime".into();
    }
    if matches!(
        base,
        "smallint"
            | "integer"
            | "bigint"
            | "int"
            | "int2"
            | "int8"
            | "real"
            | "double precision"
            | "double"
            | "float"
            | "numeric"
            | "decimal"
    ) {
        return "number".into();
    }
    if matches!(
        base,
        "text" | "varchar" | "character varying" | "char" | "character" | "clob"
    ) {
        return "text".into();
    }
    "unsupported".into()
}

pub(super) fn operators(kind: &str, readable: bool) -> Vec<FilterOperator> {
    use FilterOperator::*;
    if !readable || kind == "unsupported" {
        return vec![];
    }
    match kind {
        "text" => vec![Contains, Eq, IsNull, IsNotNull],
        "boolean" => vec![Eq, IsNull, IsNotNull],
        _ => vec![Eq, Between, Gte, Lte, IsNull, IsNotNull],
    }
}

pub(super) fn validate_request(request: &BrowseRequest) -> Result<()> {
    if request.filters.len() > MAX_FILTERS {
        return Err(invalid("最多组合 8 项筛选条件"));
    }
    if !request.filters.is_empty()
        && (request.filter_column.is_some() || request.filter_value.is_some())
    {
        return Err(invalid("filters 不能与旧筛选参数同时使用"));
    }
    if request.filter_column.is_some() != request.filter_value.is_some() {
        return Err(invalid("筛选列和筛选内容必须同时提供"));
    }
    for value in request.filter_value.iter().chain(
        request
            .filters
            .iter()
            .flat_map(|filter| filter.value.iter().chain(filter.value_to.iter())),
    ) {
        if value.len() > MAX_FILTER_BYTES {
            return Err(invalid("筛选内容不能超过 2048 字节"));
        }
    }
    for filter in &request.filters {
        let null = matches!(
            filter.operator,
            FilterOperator::IsNull | FilterOperator::IsNotNull
        );
        if null && (filter.value.is_some() || filter.value_to.is_some())
            || !null && filter.value.is_none()
            || (filter.operator == FilterOperator::Between) != filter.value_to.is_some()
        {
            return Err(invalid(
                "筛选值与操作符不匹配；范围需要上下限，空值条件不能携带值",
            ));
        }
    }
    Ok(())
}

pub(super) fn build(
    request: &BrowseRequest,
    schema: &[ColumnInspection],
    backend: DbBackend,
) -> Result<(String, Vec<SqlValue>)> {
    validate_request(request)?;
    let resolve = |name: &str| {
        schema
            .iter()
            .find(|c| c.name == name && c.readable)
            .ok_or_else(|| invalid("筛选列不存在或未开放读取"))
    };
    let mut values = vec![];
    let mut predicates = vec![];
    let contains = |column: &str, index| -> String {
        let function = if backend == DbBackend::Postgres {
            "strpos"
        } else {
            "instr"
        };
        format!(
            "{function}(CAST({column} AS TEXT), {}) > 0",
            parameter(backend, index)
        )
    };
    // 兼容旧版客户端的单列文本转换子串匹配（filter_column + filter_value）。
    if let (Some(name), Some(value)) = (&request.filter_column, &request.filter_value) {
        resolve(name)?;
        values.push(value.clone().into());
        predicates.push(contains(&identifier(name)?, values.len()));
    }
    for filter in &request.filters {
        let column = resolve(&filter.column)?;
        if !column.filter_operators.contains(&filter.operator) {
            return Err(invalid(format!("{} 不支持该筛选操作", column.name)));
        }
        let quoted = identifier(&column.name)?;
        use FilterOperator::*;
        if matches!(filter.operator, IsNull | IsNotNull) {
            predicates.push(format!(
                "{quoted} IS {}NULL",
                if filter.operator == IsNotNull {
                    "NOT "
                } else {
                    ""
                }
            ));
            continue;
        }
        let value = filter
            .value
            .as_deref()
            .ok_or_else(|| invalid("筛选缺少值"))?;
        if filter.operator == Contains {
            values.push(value.into());
            predicates.push(contains(&quoted, values.len()));
            continue;
        }
        let left = if backend == DbBackend::Sqlite
            && matches!(column.filter_kind.as_str(), "date" | "datetime")
        {
            format!("julianday({quoted})")
        } else {
            quoted
        };
        let first = bind(column, value, backend, &mut values)?;
        let predicate = match filter.operator {
            Eq => format!("{left} = {first}"),
            Gte => format!("{left} >= {first}"),
            Lte => format!("{left} <= {first}"),
            Between => {
                let second = bind(
                    column,
                    filter
                        .value_to
                        .as_deref()
                        .ok_or_else(|| invalid("范围缺少上限"))?,
                    backend,
                    &mut values,
                )?;
                format!("{left} BETWEEN {first} AND {second}")
            }
            _ => unreachable!(),
        };
        predicates.push(predicate);
    }
    Ok((
        if predicates.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", predicates.join(" AND "))
        },
        values,
    ))
}

fn bind(
    column: &ColumnInspection,
    text: &str,
    backend: DbBackend,
    values: &mut Vec<SqlValue>,
) -> Result<String> {
    let placeholder = parameter(backend, values.len() + 1);
    let bad = || {
        invalid(format!(
            "{} 的筛选值不符合 {} 类型",
            column.name, column.filter_kind
        ))
    };
    let physical = column.data_type.to_ascii_lowercase();
    let base = physical.split('(').next().unwrap_or(&physical).trim();
    let mut cast = None;
    let value = match column.filter_kind.as_str() {
        "text" => text.into(),
        "boolean" => match text {
            "true" | "1" => true.into(),
            "false" | "0" => false.into(),
            _ => return Err(bad()),
        },
        "number"
            if matches!(
                base,
                "smallint" | "integer" | "bigint" | "int" | "int2" | "int8"
            ) =>
        {
            let integer = text.parse::<i64>().map_err(|_| bad())?;
            if backend == DbBackend::Postgres
                && (matches!(base, "smallint" | "int2") && i16::try_from(integer).is_err()
                    || matches!(base, "integer" | "int") && i32::try_from(integer).is_err())
            {
                return Err(bad());
            }
            integer.into()
        }
        "number" if backend == DbBackend::Postgres && matches!(base, "numeric" | "decimal") => {
            // 直接校验十进制数字文本语法并交给数据库 CAST AS NUMERIC，避免先经 f64 二进制浮点转换产生舍入误差。
            let mantissa = text.strip_prefix(['+', '-']).unwrap_or(text);
            if mantissa.is_empty()
                || mantissa.chars().filter(|c| *c == '.').count() > 1
                || !mantissa.chars().all(|c| c.is_ascii_digit() || c == '.')
                || !mantissa.chars().any(|c| c.is_ascii_digit())
            {
                return Err(bad());
            }
            cast = Some("NUMERIC");
            text.into()
        }
        "number"
            if backend == DbBackend::Sqlite
                && matches!(base, "numeric" | "decimal")
                && text.parse::<i64>().is_ok() =>
        {
            text.parse::<i64>().map_err(|_| bad())?.into()
        }
        "number" => {
            let number = text.parse::<f64>().map_err(|_| bad())?;
            if !number.is_finite() {
                return Err(bad());
            }
            if backend == DbBackend::Postgres && base == "real" {
                let real = number as f32;
                if !real.is_finite() {
                    return Err(bad());
                }
                real.into()
            } else {
                number.into()
            }
        }
        "date" => {
            let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| bad())?;
            if date.format("%Y-%m-%d").to_string() != text || !(1..=9999).contains(&date.year()) {
                return Err(bad());
            }
            cast = Some("DATE");
            text.into()
        }
        "datetime" => {
            let parsed = DateTime::parse_from_rfc3339(text)
                .ok()
                .map(|v| v.with_timezone(&Utc))
                .or_else(|| {
                    [
                        "%Y-%m-%dT%H:%M:%S%.f",
                        "%Y-%m-%d %H:%M:%S%.f",
                        "%Y-%m-%dT%H:%M",
                        "%Y-%m-%d %H:%M",
                    ]
                    .iter()
                    .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
                    .map(|v| v.and_utc())
                })
                .ok_or_else(bad)?;
            // SQLite 的 julianday 日期函数仅精确到毫秒；遇到高于毫秒精度的输入时直接报错拒绝，绝不静默截断或四舍五入过滤边界。
            if !(1..=9999).contains(&parsed.year()) || parsed.nanosecond() >= 1_000_000_000 {
                return Err(bad());
            }
            if parsed.nanosecond() % 1_000_000 != 0 {
                return Err(invalid("时间筛选最多支持毫秒精度"));
            }
            if backend == DbBackend::Postgres && base == "timestamp without time zone" {
                cast = Some("TIMESTAMP");
                parsed.naive_utc().to_string().into()
            } else {
                cast = Some("TIMESTAMPTZ");
                parsed.to_rfc3339().into()
            }
        }
        _ => return Err(bad()),
    };
    values.push(value);
    if backend == DbBackend::Sqlite && matches!(column.filter_kind.as_str(), "date" | "datetime") {
        Ok(format!("julianday({placeholder})"))
    } else if let Some(cast) = cast {
        Ok(format!("CAST({placeholder} AS {cast})"))
    } else {
        Ok(placeholder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DatabaseColumnPolicy, DatabaseTablePolicy};
    use crate::test_support::test_db;
    use sea_orm::{ConnectionTrait, Statement};

    fn condition(
        column: &str,
        operator: FilterOperator,
        value: Option<&str>,
        value_to: Option<&str>,
    ) -> BrowseFilter {
        BrowseFilter {
            column: column.into(),
            operator,
            value: value.map(str::to_owned),
            value_to: value_to.map(str::to_owned),
        }
    }
    fn request(filters: Vec<BrowseFilter>) -> BrowseRequest {
        BrowseRequest {
            filters,
            ..Default::default()
        }
    }
    async fn run(db: &Database, sql: &str) {
        db.orm()
            .execute_raw(Statement::from_string(db.backend(), sql))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn typed_filters_preserve_numeric_precision_and_combine_null_text_boolean_and_dates() {
        use FilterOperator::*;
        let db = test_db(2).await;
        let datetime = if db.backend() == DbBackend::Postgres {
            "TIMESTAMP WITH TIME ZONE"
        } else {
            "DATETIME"
        };
        run(&db, &format!("CREATE TABLE inspection_typed (id BIGINT PRIMARY KEY, label TEXT, enabled BOOLEAN, score REAL, exact NUMERIC, day DATE, at {datetime}, secret TEXT)")).await;
        run(&db, "INSERT INTO inspection_typed VALUES (9007199254740993, 'literal %_ needle', TRUE, 12.1, 123.456, '2026-10-01', '2026-10-01T08:00:00+08:00', 'hidden'), (9007199254740994, 'other', FALSE, 2.0, 9007199254740993, '2026-10-02', '2026-10-02T00:00:00Z', 'hidden'), (9007199254740995, NULL, NULL, NULL, NULL, NULL, NULL, 'hidden')").await;
        let policy = DatabaseTablePolicy::read(
            "inspection_typed",
            ["id", "label", "enabled", "score", "exact", "day", "at"]
                .into_iter()
                .map(DatabaseColumnPolicy::read)
                .collect(),
        );
        for (filters, expected) in [
            (
                vec![condition("exact", Eq, Some("9007199254740993"), None)],
                1,
            ),
            (vec![condition("score", Eq, Some("12.1"), None)], 1),
            (vec![condition("id", Eq, Some("9007199254740993"), None)], 1),
            (
                vec![condition(
                    "id",
                    Between,
                    Some("9007199254740993"),
                    Some("9007199254740994"),
                )],
                2,
            ),
            (
                vec![
                    condition("label", Contains, Some("%_"), None),
                    condition("enabled", Eq, Some("true"), None),
                    condition("score", Gte, Some("10"), None),
                ],
                1,
            ),
            (vec![condition("label", Eq, Some("other"), None)], 1),
            (vec![condition("label", IsNull, None, None)], 1),
            (vec![condition("label", IsNotNull, None, None)], 2),
            (
                vec![condition(
                    "day",
                    Between,
                    Some("2026-10-01"),
                    Some("2026-10-02"),
                )],
                2,
            ),
            (
                vec![condition("at", Eq, Some("2026-10-01T00:00:00Z"), None)],
                1,
            ),
            (
                vec![condition("at", Lte, Some("2026-10-01T00:00"), None)],
                1,
            ),
            (
                vec![condition(
                    "at",
                    Between,
                    Some("2026-10-01T00:00:00Z"),
                    Some("2026-10-02T00:00:00Z"),
                )],
                2,
            ),
            (vec![condition("exact", Eq, Some("123.456"), None)], 1),
            (
                vec![condition("label", Contains, Some("' OR 1=1 --"), None)],
                0,
            ),
        ] {
            let request = request(filters);
            let page = db
                .browse_table_with_policy("inspection_typed", &request, Some(&policy))
                .await
                .unwrap();
            assert_eq!(page.total, expected, "{:?}", request.filters);
            let export = db
                .export_table_with_policy("inspection_typed", &request, Some(&policy))
                .await
                .unwrap();
            assert_eq!(page.rows, export.rows);
        }
        let core = db.inspect_table("nuono_users").await.unwrap();
        assert_eq!(
            core.columns
                .iter()
                .find(|c| c.name == "created_at_ms")
                .unwrap()
                .filter_kind,
            "number"
        );
        assert_eq!(
            core.columns
                .iter()
                .find(|c| c.name == "totp_enabled")
                .unwrap()
                .filter_kind,
            "boolean"
        );
        assert!(
            core.columns
                .iter()
                .find(|c| c.name == "password_hash")
                .unwrap()
                .filter_operators
                .is_empty()
        );
        let schema = db
            .inspect_table_with_policy("inspection_typed", Some(&policy))
            .await
            .unwrap();
        assert!(schema.table.row_count.is_none());
        assert!(
            schema
                .columns
                .iter()
                .find(|c| c.name == "secret")
                .unwrap()
                .filter_operators
                .is_empty()
        );
        for filter in [
            condition("id", Eq, Some("9007199254740993.0"), None),
            condition("id", Eq, Some("9223372036854775808"), None),
            condition("id", Eq, Some("garbage"), None),
            condition("score", Eq, Some("NaN"), None),
            condition("score", Eq, Some("inf"), None),
            condition("enabled", Eq, Some("yes"), None),
            condition("enabled", Contains, Some("true"), None),
            condition("day", Eq, Some("2026-02-30"), None),
            condition("at", Eq, Some("yesterday"), None),
            condition("at", Eq, Some("2026-10-01T00:00:00.000001Z"), None),
            condition("exact", Eq, Some("0 OR 1=1"), None),
            condition("secret", IsNotNull, None, None),
            condition("id; DROP TABLE nuono_users", Eq, Some("1"), None),
        ] {
            let result = db
                .browse_table_with_policy(
                    "inspection_typed",
                    &request(vec![filter.clone()]),
                    Some(&policy),
                )
                .await;
            assert!(
                matches!(result, Err(crate::error::NuonoError::InvalidInput(_))),
                "{filter:?}: {result:?}"
            );
        }
        db.shutdown().await.unwrap();
    }

    #[test]
    fn filter_shapes_and_boundaries_are_checked_before_querying() {
        use FilterOperator::*;
        let valid = condition("label", Eq, Some("text"), None);
        assert!(validate_request(&request(vec![valid.clone(); 8])).is_ok());
        assert!(validate_request(&request(vec![valid.clone(); 9])).is_err());
        for filter in [
            condition("label", Eq, None, None),
            condition("label", Eq, Some("text"), Some("extra")),
            condition("label", Between, Some("1"), None),
            condition("label", IsNull, Some("text"), None),
            condition("label", Eq, Some(&"大".repeat(683)), None),
        ] {
            assert!(validate_request(&request(vec![filter])).is_err());
        }
        assert!(
            validate_request(&BrowseRequest {
                filters: vec![valid],
                filter_column: Some("label".into()),
                filter_value: Some("text".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            serde_json::from_str::<BrowseFilter>(r#"{"column":"id","operator":"eq","value":1}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<BrowseFilter>(r#"{"column":"id","operator":"sql","value":"1"}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<BrowseFilter>(
                r#"{"column":"id","operator":"eq","value":"1","sql":"evil"}"#
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn export_rejects_count_before_reading_oversized_payloads() {
        let db = test_db(2).await;
        run(&db, "WITH RECURSIVE entries(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM entries WHERE n < 5001) INSERT INTO nuono_filter_rules (pattern, target_id, filter_type) SELECT 'small', 'bulk', 'receive' FROM entries").await;
        let statement = format!(
            "UPDATE nuono_filter_rules SET pattern = {} WHERE id = (SELECT MIN(id) FROM nuono_filter_rules)",
            parameter(db.backend(), 1)
        );
        db.orm()
            .execute_raw(Statement::from_sql_and_values(
                db.backend(),
                statement,
                vec![SqlValue::from(
                    "a".repeat(super::super::MAX_CELL_BYTES as usize + 1),
                )],
            ))
            .await
            .unwrap();
        let result = db
            .export_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    limit: super::super::MAX_EXPORT_LIMIT,
                    ..Default::default()
                },
            )
            .await;
        assert!(
            matches!(result, Err(crate::error::NuonoError::InvalidInput(message)) if message.contains("5001") && message.contains("最多导出"))
        );
        // 即使表中存在超大字段行，行数统计与表结构元数据查询也必须保持可用（先校验导出总行数上限，再读取单元格内容）。
        run(
            &db,
            "DELETE FROM nuono_filter_rules WHERE id = (SELECT MIN(id) FROM nuono_filter_rules)",
        )
        .await;
        let exactly_at_limit = db
            .export_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    limit: super::super::MAX_EXPORT_LIMIT,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(exactly_at_limit.total, 5000);
        assert_eq!(exactly_at_limit.rows.len(), 5000);
        assert!(
            db.inspect_tables()
                .await
                .unwrap()
                .iter()
                .all(|table| table.row_count.is_none())
        );
        assert!(
            db.inspect_table("nuono_filter_rules")
                .await
                .unwrap()
                .table
                .row_count
                .is_none()
        );
        db.shutdown().await.unwrap();
    }
}
