//! 通用数据库记录增删改管理：基于显式可写列白名单与全字段乐观并发比对，
//! 并在同一写事务内原子提交业务数据变更、持久化幂等回执与操作审计。
use sea_orm::{
    ActiveValue::Set, ConnectionTrait, DbBackend, EntityTrait, Statement, Value as SqlValue,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{
    ColumnInspection, Database,
    inspection::{identifier, parameter, qualified_table},
    policy::{self, DatabaseTablePolicy},
};
use crate::{
    entity::audit_events,
    error::{NuonoError, Result},
};

const MUTATION_KEY_PREFIX: &str = "db_mutation.";

#[derive(Serialize, Deserialize)]
struct StoredMutationReceipt {
    actor: String,
    request_digest: String,
    result: MutationResult,
}

fn mutation_config_key(request_id: &str) -> String {
    format!("{MUTATION_KEY_PREFIX}{request_id}")
}

#[cfg(test)]
#[path = "../../tests/unit/db/management/plugin_tests.rs"]
mod plugin_tests;
#[cfg(test)]
#[path = "../../tests/unit/db/management/tests.rs"]
mod tests;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TableManagement {
    pub create: bool,
    pub update: bool,
    pub delete: bool,
}

impl TableManagement {
    pub(super) fn for_table(table: &str) -> Self {
        let enabled = !writable_columns(table).is_empty();
        Self {
            create: enabled,
            update: enabled,
            delete: enabled,
        }
    }

    pub(super) fn for_schema(table: &str, columns: &[ColumnInspection]) -> Self {
        Self::for_table(table).with_schema(columns)
    }

    pub(super) fn with_schema(mut self, columns: &[ColumnInspection]) -> Self {
        let result = &mut self;
        let keys: Vec<_> = columns.iter().filter(|column| column.primary_key).collect();
        let keyed = !keys.is_empty() && keys.iter().all(|column| column.readable);
        result.update &= keyed && columns.iter().any(|column| column.writable);
        result.delete &= keyed;
        result.create &= keyed
            && columns.iter().all(|column| {
                column.nullable || column.generated || column.has_default || column.insertable
            });
        self
    }
}

pub(super) fn writable_columns(table: &str) -> &'static [&'static str] {
    match table {
        "nuono_filter_rules" => &[
            "filter_type",
            "pattern",
            "match_mode",
            "target_id",
            "enabled",
        ],
        _ => &[],
    }
}

pub(super) fn edit_type(table: &str, column: &str, data_type: &str) -> Option<&'static str> {
    if !writable_columns(table).contains(&column) {
        return None;
    }
    let data_type = data_type.to_lowercase();
    if table == "nuono_filter_rules"
        && column == "enabled"
        && (data_type.contains("bool") || data_type == "integer")
    {
        Some("boolean")
    } else if data_type.contains("text") || data_type.contains("char") {
        Some("text")
    } else {
        None
    }
}

pub(super) fn field_max_length(table: &str, column: &str) -> Option<u64> {
    match (table, column) {
        ("nuono_filter_rules", "match_mode") => Some(10),
        ("nuono_filter_rules", "target_id") => Some(50),
        ("nuono_filter_rules", "filter_type") => Some(20),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MutationOperation {
    Create,
    Update,
    Delete,
}

impl MutationOperation {
    fn label(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MutationRequest {
    /// 每次编辑操作对应的唯一幂等请求 UUID；仅在网络超时重试同一请求内容时复用。
    pub request_id: String,
    #[serde(default)]
    pub values: Map<String, Value>,
    /// 浏览接口返回的原记录快照（各可读列的精确文本或 `null`）；执行更新或删除时必填，用于乐观并发比对。
    #[serde(default)]
    pub original: Option<Map<String, Value>>,
    /// 删除确认表名（必须与目标表名完全一致）；执行删除时必填。
    #[serde(default)]
    pub confirmation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MutationResult {
    pub table: String,
    pub operation: String,
    pub affected_rows: u64,
    pub key: Map<String, Value>,
}

fn invalid(message: impl Into<String>) -> NuonoError {
    NuonoError::InvalidInput(message.into())
}

fn write_error(error: sea_orm::DbErr) -> NuonoError {
    match error.sql_err() {
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_)) => {
            invalid("记录与已有数据重复，请检查唯一字段")
        }
        Some(sea_orm::SqlErr::ForeignKeyConstraintViolation(_)) => {
            invalid("记录的关联约束不满足，请先检查引用关系")
        }
        _ => NuonoError::DatabaseQuery(
            "数据或审计写入失败，本次操作未提交；请检查字段约束与数据库状态".into(),
        ),
    }
}

fn value_binding(
    column: &ColumnInspection,
    value: &Value,
    backend: DbBackend,
) -> Result<(SqlValue, &'static str)> {
    if value.is_null() {
        if !column.nullable || column.primary_key {
            return Err(invalid(format!("{} 不允许 NULL", column.name)));
        }
        return Ok((
            SqlValue::String(None),
            if column.edit_type.as_deref() == Some("boolean") {
                "BOOLEAN"
            } else if column.edit_type.as_deref() == Some("integer") {
                "BIGINT"
            } else if column.data_type == "jsonb" {
                "JSONB"
            } else if column.data_type == "json" {
                "JSON"
            } else {
                "TEXT"
            },
        ));
    }
    if column.edit_type.as_deref() == Some("boolean") {
        let value = match value {
            Value::Bool(value) => *value,
            Value::String(value) if matches!(value.as_str(), "true" | "1") => true,
            Value::String(value) if matches!(value.as_str(), "false" | "0") => false,
            _ => return Err(invalid(format!("{} 必须为 true 或 false", column.name))),
        };
        return Ok((value.into(), "BOOLEAN"));
    }
    if column.edit_type.as_deref() == Some("integer") {
        let text = value
            .as_str()
            .ok_or_else(|| invalid(format!("{} 必须使用整数文本，避免精度丢失", column.name)))?;
        let integer = text
            .parse::<i64>()
            .map_err(|_| invalid(format!("{} 必须为 64 位范围内的整数", column.name)))?;
        if backend == DbBackend::Postgres
            && ((column.data_type == "smallint" && i16::try_from(integer).is_err())
                || (column.data_type == "integer" && i32::try_from(integer).is_err()))
        {
            return Err(invalid(format!("{} 超过数据库整数范围", column.name)));
        }
        return Ok((integer.to_string().into(), "BIGINT"));
    }
    if column.edit_type.as_deref() == Some("json") {
        let text = value
            .as_str()
            .ok_or_else(|| invalid(format!("{} 必须为 JSON 文本", column.name)))?;
        if text.len() > 65_536
            || column
                .max_length
                .is_some_and(|max| text.chars().count() as u64 > max)
            || serde_json::from_str::<Value>(text).is_err()
        {
            return Err(invalid(format!(
                "{} 必须为有效且符合长度限制的 JSON",
                column.name
            )));
        }
        return Ok((
            text.into(),
            match column.data_type.as_str() {
                "jsonb" => "JSONB",
                "json" => "JSON",
                _ => "TEXT",
            },
        ));
    }
    let data_type = column.data_type.to_lowercase();
    if !data_type.contains("text") && !data_type.contains("char") {
        return Err(invalid(format!(
            "{} 的字段类型暂不支持通用编辑",
            column.name
        )));
    }
    let value = value
        .as_str()
        .ok_or_else(|| invalid(format!("{} 必须为文本", column.name)))?;
    if value.len() > 65_536
        || column
            .max_length
            .is_some_and(|limit| value.chars().count() as u64 > limit)
    {
        return Err(invalid(format!("{} 超过字段长度限制", column.name)));
    }
    if (!column.nullable || column.primary_key) && value.trim().is_empty() {
        return Err(invalid(format!("{} 不能为空", column.name)));
    }
    Ok((value.into(), "TEXT"))
}

fn validate_business(table: &str, values: &Map<String, Value>) -> Result<()> {
    let text = |name: &str| values.get(name).and_then(Value::as_str);
    if table == "nuono_filter_rules" {
        if let Some(filter_type) = text("filter_type") {
            if !matches!(
                filter_type,
                "receive" | "send" | "blacklist_user" | "blacklist_group"
            ) {
                return Err(invalid(
                    "filter_type 仅支持 receive、send、blacklist_user、blacklist_group",
                ));
            }
            if matches!(filter_type, "blacklist_user" | "blacklist_group")
                && text("target_id").is_none_or(|id| id.trim().is_empty())
            {
                return Err(invalid("黑名单规则必须填写 target_id"));
            }
        }
        if text("match_mode")
            .is_some_and(|mode| !matches!(mode, "simple" | "regex" | "logic" | "exact"))
        {
            return Err(invalid("match_mode 仅支持 simple、regex、logic、exact"));
        }
        if text("match_mode") == Some("regex") {
            let pattern = text("pattern").ok_or_else(|| invalid("正则规则必须填写 pattern"))?;
            regex::RegexBuilder::new(pattern)
                .size_limit(1024 * 1024)
                .build()
                .map_err(|_| invalid("pattern 不是有效或可接受大小的正则表达式"))?;
        }
    }
    Ok(())
}

// 序列化时显式对每个 JSON 对象的键按字典序排序：避免因插件依赖树开启了 serde_json/preserve_order 特性导致请求摘要不一致。
fn canonical_json(value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match value {
        Value::Object(object) => {
            out.push(b'{');
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(key)?);
                out.push(b':');
                canonical_json(&object[key], out)?;
            }
            out.push(b'}');
        }
        Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                canonical_json(value, out)?;
            }
            out.push(b']');
        }
        _ => out.extend(serde_json::to_vec(value)?),
    }
    Ok(())
}

fn check_receipt_policy(
    result: &MutationResult,
    policy: Option<&DatabaseTablePolicy>,
) -> Result<()> {
    if policy.is_some_and(|policy| {
        result
            .key
            .keys()
            .any(|name| !policy.columns.iter().any(|column| column.name == *name))
    }) {
        return Err(NuonoError::PermissionDenied(
            "当前插件策略未开放该操作回执的主键字段".into(),
        ));
    }
    Ok(())
}

async fn replay_receipt<C: ConnectionTrait>(
    connection: &C,
    request_id: &str,
    actor: &str,
    digest: &str,
) -> Result<Option<MutationResult>> {
    let backend = connection.get_database_backend();
    let receipt = connection
        .query_one_raw(Statement::from_sql_and_values(
            backend,
            format!(
                "SELECT value_json FROM {} WHERE key = {}",
                qualified_table(backend, "nuono_global_configs")?,
                parameter(backend, 1)
            ),
            [mutation_config_key(request_id).into()],
        ))
        .await?;
    if let Some(receipt) = receipt {
        let stored: StoredMutationReceipt =
            serde_json::from_str(&receipt.try_get::<String>("", "value_json")?)?;
        if stored.actor != actor || stored.request_digest != digest {
            return Err(NuonoError::Conflict(
                "该操作编号已用于其他请求，请重新打开编辑窗口".into(),
            ));
        }
        return Ok(Some(stored.result));
    }
    Ok(None)
}

impl Database {
    pub async fn mutate_table(
        &self,
        table: &str,
        operation: MutationOperation,
        request: &MutationRequest,
        actor: &str,
    ) -> Result<MutationResult> {
        self.mutate_table_with_policy(table, operation, request, actor, None)
            .await
    }

    pub async fn mutate_table_with_policy(
        &self,
        table: &str,
        operation: MutationOperation,
        request: &MutationRequest,
        actor: &str,
        declared_policy: Option<&DatabaseTablePolicy>,
    ) -> Result<MutationResult> {
        policy::check_policy(table, declared_policy)?;
        let request_id = uuid::Uuid::parse_str(&request.request_id)
            .map_err(|_| invalid("request_id 必须为 UUID"))?
            .to_string();
        if actor.trim().is_empty() {
            return Err(NuonoError::PermissionDenied("缺少操作身份".into()));
        }
        if serde_json::to_vec(request)?.len() > 2 * 1024 * 1024 {
            return Err(invalid("操作内容超过 2 MiB"));
        }
        if writable_columns(table).is_empty()
            && declared_policy.is_none_or(|policy| match operation {
                MutationOperation::Create => !policy.create,
                MutationOperation::Update => !policy.update,
                MutationOperation::Delete => !policy.delete,
            })
        {
            return Err(NuonoError::PermissionDenied(
                "此表不开放记录操作，请使用对应的业务管理页面".into(),
            ));
        }
        let mut canonical_request = serde_json::to_value((table, operation, request))?;
        canonical_request[2]["request_id"] = Value::String(request_id.clone());
        let mut encoded = Vec::new();
        canonical_json(&canonical_request, &mut encoded)?;
        let digest = hex::encode(Sha256::digest(encoded));
        // 幂等重放：即便首次提交后表结构或业务校验规则发生了变化，只要命中已提交回执就直接返回原结果；下方进入写事务加锁后还会二次复查。
        if let Some(result) = replay_receipt(self.orm(), &request_id, actor, &digest).await? {
            check_receipt_policy(&result, declared_policy)?;
            return Ok(result);
        }
        let columns = self
            .inspection_columns_with_policy(table, declared_policy)
            .await?;
        let policy = policy::management(table, &columns, declared_policy);
        let allowed = match operation {
            MutationOperation::Create => policy.create,
            MutationOperation::Update => policy.update,
            MutationOperation::Delete => policy.delete,
        };
        if !allowed {
            return Err(NuonoError::PermissionDenied(
                "此表不开放该记录操作，请使用对应的业务管理页面".into(),
            ));
        }
        let create = matches!(operation, MutationOperation::Create);
        let delete = matches!(operation, MutationOperation::Delete);
        if create && request.original.is_some()
            || !create && request.original.is_none()
            || !delete && request.confirmation.is_some()
            || delete && request.confirmation.as_deref() != Some(table)
            || delete && !request.values.is_empty()
            || !delete && !create && request.values.is_empty()
        {
            return Err(invalid(
                "操作缺少记录快照、修改字段或删除确认，或包含不适用的参数",
            ));
        }
        let mut bindings = Vec::<SqlValue>::new();
        let mut names = Vec::new();
        let mut expressions = Vec::new();
        for (name, value) in &request.values {
            let column = columns
                .iter()
                .find(|column| {
                    column.name == *name
                        && if create {
                            column.insertable
                        } else {
                            column.writable
                        }
                })
                .ok_or_else(|| invalid(format!("{name} 不允许修改")))?;
            let (binding, cast) = value_binding(column, value, self.backend())?;
            bindings.push(binding);
            names.push(identifier(name)?);
            expressions.push(format!(
                "CAST({} AS {cast})",
                parameter(self.backend(), bindings.len())
            ));
        }
        if create {
            for column in &columns {
                if column.insertable
                    && (!column.nullable || column.primary_key)
                    && !column.generated
                    && !column.has_default
                    && !request.values.contains_key(&column.name)
                {
                    return Err(invalid(format!("必须填写 {}", column.name)));
                }
            }
        }
        let mut candidate = request.original.clone().unwrap_or_default();
        candidate.extend(request.values.clone());
        if !delete {
            validate_business(table, &candidate)?;
        }
        let mut predicates = Vec::new();
        if let Some(original) = &request.original {
            let readable: Vec<_> = columns.iter().filter(|column| column.readable).collect();
            if original.len() != readable.len()
                || readable
                    .iter()
                    .any(|column| !original.contains_key(&column.name))
            {
                return Err(invalid("记录快照与当前表结构不一致，请刷新后重试"));
            }
            for column in readable {
                let value = &original[&column.name];
                if !value.is_null() && !value.is_string() || column.primary_key && value.is_null() {
                    return Err(invalid("快照值必须是浏览返回的文本或 NULL，主键不可为空"));
                }
                bindings.push(value.as_str().map(String::from).into());
                let comparison = if self.backend() == DbBackend::Postgres {
                    "IS NOT DISTINCT FROM"
                } else {
                    "IS"
                };
                predicates.push(format!(
                    "CAST({} AS TEXT) {comparison} {}",
                    identifier(&column.name)?,
                    parameter(self.backend(), bindings.len())
                ));
            }
        }
        let table_sql = qualified_table(self.backend(), table)?;
        let returning = columns
            .iter()
            .filter(|column| column.primary_key)
            .map(|column| {
                let name = identifier(&column.name)?;
                Ok(format!("CAST({name} AS TEXT) AS {name}"))
            })
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        let statement = if create && names.is_empty() {
            format!("INSERT INTO {table_sql} DEFAULT VALUES RETURNING {returning}")
        } else if create {
            format!(
                "INSERT INTO {table_sql} ({}) VALUES ({}) RETURNING {returning}",
                names.join(", "),
                expressions.join(", ")
            )
        } else if delete {
            format!(
                "DELETE FROM {table_sql} WHERE {} RETURNING {returning}",
                predicates.join(" AND ")
            )
        } else {
            let assignments = names
                .iter()
                .zip(expressions)
                .map(|(name, value)| format!("{name} = {value}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "UPDATE {table_sql} SET {assignments} WHERE {} RETURNING {returning}",
                predicates.join(" AND ")
            )
        };
        let tx = self.begin_write().await?;
        if self.backend() == DbBackend::Postgres {
            tx.execute_unprepared("SET LOCAL statement_timeout = '10s'")
                .await?;
            tx.execute_unprepared("SET LOCAL lock_timeout = '3s'")
                .await?;
            tx.query_one_raw(Statement::from_sql_and_values(
                self.backend(),
                "SELECT pg_advisory_xact_lock(hashtextextended($1, 846302))",
                [request_id.clone().into()],
            ))
            .await?;
        }
        if let Some(result) = replay_receipt(&tx, &request_id, actor, &digest).await? {
            check_receipt_policy(&result, declared_policy)?;
            tx.commit().await?;
            return Ok(result);
        }
        let rows = tx
            .query_all_raw(Statement::from_sql_and_values(
                self.backend(),
                statement,
                bindings,
            ))
            .await
            .map_err(write_error)?;
        if rows.len() != 1 {
            return Err(NuonoError::Conflict(
                "记录已被修改或删除，请刷新后重新编辑".into(),
            ));
        }
        let mut key = Map::new();
        for column in columns.iter().filter(|column| column.primary_key) {
            let value: String = rows[0].try_get("", &column.name)?;
            key.insert(column.name.clone(), Value::String(value));
        }
        let result = MutationResult {
            table: table.into(),
            operation: operation.label().into(),
            affected_rows: 1,
            key,
        };
        let metadata = serde_json::json!({"table": table, "operation": operation.label(), "key": result.key, "request_id": request.request_id, "fields": request.values.keys().collect::<Vec<_>>()});
        let now = crate::clock::db_now();
        audit_events::Entity::insert(audit_events::ActiveModel {
            category: Set(crate::audit::CATEGORY_EVENT.to_string()),
            occurred_at_ms: Set(now.timestamp_millis()),
            action: Set(format!("database.record_{}", operation.label())),
            bot_id: Set(None),
            details_json: Set(metadata.to_string()),
            actor_label: Set(Some(actor.into())),
            result: Set("ok".into()),
            ..Default::default()
        })
        .exec(&tx)
        .await
        .map_err(write_error)?;
        let stored_receipt = serde_json::to_string(&StoredMutationReceipt {
            actor: actor.into(),
            request_digest: digest,
            result: result.clone(),
        })?;
        let now_ms = now.timestamp_millis();
        tx.execute_raw(Statement::from_sql_and_values(
            self.backend(),
            format!(
                "INSERT INTO {} (key, value_json, value_version, revision, created_at_ms, updated_at_ms) VALUES ({}, {}, 1, 1, {}, {})",
                qualified_table(self.backend(), "nuono_global_configs")?,
                parameter(self.backend(), 1),
                parameter(self.backend(), 2),
                parameter(self.backend(), 3),
                parameter(self.backend(), 4),
            ),
            vec![
                mutation_config_key(&request_id).into(),
                stored_receipt.into(),
                now_ms.into(),
                now_ms.into(),
            ],
        ))
        .await
        .map_err(write_error)?;
        // 清理超过 30 天的历史幂等防重放回执。
        tx.execute_raw(Statement::from_sql_and_values(
            self.backend(),
            format!(
                "DELETE FROM {} WHERE key LIKE 'db_mutation.%' AND created_at_ms < {}",
                qualified_table(self.backend(), "nuono_global_configs")?,
                parameter(self.backend(), 1)
            ),
            [(now - chrono::Duration::days(30)).timestamp_millis().into()],
        ))
        .await
        .map_err(write_error)?;
        tx.commit().await.map_err(|_| {
            NuonoError::DatabaseQuery("提交结果尚未确认，请保留原操作编号重试确认结果".into())
        })?;
        Ok(result)
    }
}
