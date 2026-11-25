//! 数据库只读浏览与巡检：采用白名单机制，只返回显式声明允许展示的列。
//!
//! 表结构（schema）发现涵盖所有业务表、插件表与历史表；但读取表内记录是独立受控的权限：
//! 只有下方白名单显式审核放行的列才允许查询、排序或筛选，给表新增列绝不会自动对外暴露。

mod filters;
pub use filters::{BrowseFilter, FilterOperator};

use sea_orm::{
    AccessMode, ConnectionTrait, DatabaseTransaction, DbBackend, IsolationLevel, Statement,
    TransactionTrait, Value as SqlValue,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    Database,
    management::{TableManagement, edit_type, field_max_length, writable_columns},
    policy::{self, DatabaseTablePolicy},
};
use crate::error::{NuonoError, Result};

pub const MAX_BROWSE_LIMIT: u64 = 200;
pub const MAX_EXPORT_LIMIT: u64 = 5_000;
const MAX_OFFSET: u64 = 1_000_000;
const MAX_FILTER_BYTES: usize = 2_048;
const MAX_CELL_BYTES: i64 = 1024 * 1024;
const MAX_PAGE_BYTES: i64 = 16 * 1024 * 1024;

/// 表归属分类（仅用于管理界面展示，不影响巡检器的列读取白名单策略）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TableOwner {
    Core,
    Plugin {
        name: String,
    },
    #[default]
    Unassigned,
}

impl TableOwner {
    pub fn for_table(name: &str) -> Self {
        table_owner(name)
    }
}

// 由宿主核心迁移维护的固定表集合（含历史表）；
// 未声明归属的插件表或外部表统一标记为 Unassigned。
pub(super) fn table_owner(name: &str) -> TableOwner {
    if matches!(
        name,
        "nuono_sessions"
            | "nuono_account_merge_jobs"
            | "nuono_groups"
            | "nuono_group_bots"
            | "nuono_plugin_overrides"
            | "nuono_deployment"
            | "nuono_schema_migrations"
            | "nuono_platform_identities"
            | "nuono_account_challenges"
            | "nuono_audit_events"
            | "nuono_entitlements"
            | "nuono_group_authorizations"
            | "nuono_account_assistant"
            | "nuono_assistant_turns"
            | "nuono_companion_memory_sources"
            | "nuono_agent_memories"
            | "nuono_agent_memory_embeddings"
            | "nuono_agent_memory_graphs"
            | "nuono_agent_policy_rules"
            | "nuono_agent_runs"
            | "nuono_agent_tool_approvals"
            | "nuono_agent_tool_receipts"
            | "nuono_ai_models"
            | "nuono_ai_runs"
            | "nuono_ai_usage_events"
            | "nuono_activity_daily"
            | "nuono_ai_personas"
            | "nuono_ai_persona_bindings"
            | "nuono_bot_instances"
            | "nuono_conversations"
            | "nuono_filter_rules"
            | "nuono_global_configs"
            | "nuono_messages"
            | "nuono_notifications"
            | "nuono_plugin_kv"
            | "nuono_plugin_migrations"
            | "nuono_plugins"
            | "nuono_runtime_secrets"
            | "nuono_scheduled_tasks"
            | "nuono_scheduled_task_runs"
            | "nuono_feed_subscriptions"
            | "nuono_users"
    ) {
        TableOwner::Core
    } else {
        TableOwner::Unassigned
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TableInspection {
    pub name: String,
    #[serde(default)]
    pub owner: TableOwner,
    /// 表目录与表结构发现阶段不扫描表行数（固定为 `None`）；实际匹配行数见分页查询返回的 `BrowsePage.total`。
    pub row_count: Option<u64>,
    pub browsable: bool,
    #[serde(default)]
    pub management: TableManagement,
    pub restriction: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ColumnInspection {
    pub name: String,
    pub data_type: String,
    pub filter_kind: String,
    pub filter_operators: Vec<FilterOperator>,
    pub nullable: bool,
    pub primary_key: bool,
    pub readable: bool,
    pub writable: bool,
    pub insertable: bool,
    pub edit_type: Option<String>,
    pub default_value: Option<String>,
    pub has_default: bool,
    pub generated: bool,
    pub max_length: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TableSchema {
    pub table: TableInspection,
    pub columns: Vec<ColumnInspection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BrowseRequest {
    pub offset: u64,
    pub limit: u64,
    pub sort: Option<String>,
    pub descending: bool,
    /// 按 AND 组合的多条件过滤器；不能与旧版单列过滤参数（`filter_column` / `filter_value`）同时使用。
    pub filters: Vec<BrowseFilter>,
    pub filter_column: Option<String>,
    /// 区分大小写的字面量子串匹配（`%` 与 `_` 均按普通字符处理，不作为通配符）。
    pub filter_value: Option<String>,
}

impl Default for BrowseRequest {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
            sort: None,
            descending: false,
            filters: vec![],
            filter_column: None,
            filter_value: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BrowsePage {
    pub table: String,
    pub columns: Vec<String>,
    /// 每个单元格的值统一序列化为字符串或 `null`：以文本形式传输可防止 JavaScript 丢失 64 位大整数精度，并保证双数据库后端返回格式一致。
    pub rows: Vec<Value>,
    pub total: u64,
    pub offset: u64,
    pub limit: u64,
}

/// 核心表可读列白名单：刻意排除密码哈希与密钥凭据、任意配置/KV 载荷、原始协议消息及详细日志上下文。
/// 未知表或插件扩展表在宿主侧未显式提供列策略前，仅暴露表结构元数据，绝不从表名、插件自带声明或用户可编辑的库内数据自动推导可读列。
fn readable_columns(table: &str) -> &'static [&'static str] {
    match table {
        "nuono_users" => &[
            "id",
            "account_kind",
            "username",
            "email",
            "email_verified_at_ms",
            "role",
            "status",
            "totp_enabled",
            "last_login_at_ms",
            "created_at_ms",
            "updated_at_ms",
        ],
        "nuono_bot_instances" => &[
            "id",
            "name",
            "platform",
            "namespace",
            "adapter_type",
            "native_bot_id",
            "enabled",
            "runtime_status",
            "last_error",
            "last_connected_at_ms",
            "revision",
            "created_at_ms",
            "updated_at_ms",
        ],
        "nuono_ai_models" => &[
            "id",
            "name",
            "provider",
            "provider_type",
            "model_id",
            "is_default",
            "enabled",
            "created_at",
            "updated_at",
        ],
        "nuono_plugins" => &[
            "plugin_id",
            "enabled",
            "allow_user_overrides",
            "allow_group_overrides",
            "config_version",
            "revision",
            "created_at_ms",
            "updated_at_ms",
        ],
        "nuono_groups" => &[
            "id",
            "platform",
            "namespace",
            "conversation_kind",
            "native_parent_id",
            "native_group_id",
            "display_name",
            "enabled",
            "revision",
            "created_at_ms",
            "updated_at_ms",
        ],
        "nuono_group_bots" => &["group_id", "platform", "bot_id", "synced_at_ms"],
        "nuono_plugin_overrides" => &[
            "id",
            "plugin_id",
            "user_id",
            "group_id",
            "enabled",
            "revision",
            "created_at_ms",
            "updated_at_ms",
        ],
        "nuono_messages" => &[
            "id",
            "conversation_id",
            "platform",
            "bot_instance_id",
            "native_message_id",
            "native_sender_id",
            "native_group_id",
            "direction",
            "role",
            "occurred_at_ms",
            "created_at_ms",
        ],
        "nuono_entitlements" => &[
            "id",
            "subject_type",
            "subject_id",
            "feature_key",
            "source",
            "source_ref",
            "status",
            "starts_at_ms",
            "expires_at_ms",
            "updated_at_ms",
        ],
        "nuono_feed_subscriptions" => &[
            "id",
            "subscriber_id",
            "topic",
            "target_id",
            "enabled",
            "created_at",
        ],
        "nuono_scheduled_tasks" => &[
            "id",
            "name",
            "cron_expr",
            "action_type",
            "bot_id",
            "enabled",
            "last_run_at",
            "created_at",
        ],
        "nuono_filter_rules" => &[
            "id",
            "filter_type",
            "pattern",
            "match_mode",
            "target_id",
            "enabled",
            "created_at",
        ],
        "nuono_notifications" => &["id", "user_id", "kind", "title", "read_at", "created_at"],
        "nuono_audit_events" => &[
            "id",
            "category",
            "occurred_at_ms",
            "action",
            "bot_id",
            "actor_label",
            "result",
        ],
        _ => &[],
    }
}

fn restriction(table: &str) -> Option<String> {
    if !writable_columns(table).is_empty() {
        Some("可管理业务记录；保存和删除会检查并发修改，并记录操作审计".into())
    } else if readable_columns(table).is_empty() {
        Some("此表仅开放结构查看；凭据、配置及未经审核的扩展表不开放记录浏览或导出".into())
    } else {
        Some("仅显示已审核的列；密钥、配置及原始载荷等字段不会进入查询或导出".into())
    }
}

fn invalid(message: impl Into<String>) -> NuonoError {
    NuonoError::InvalidInput(message.into())
}

/// 对 SQL 标识符加双引号转义，以安全处理包含空格或引号的已有表名/列名。
/// 调用前名称仍须与数据库实际发现的表/列匹配，绝不将未经校验的用户输入直接拼入 SQL。
pub(super) fn identifier(name: &str) -> Result<String> {
    if name.is_empty() || name.len() > 255 || name.contains('\0') {
        return Err(invalid("数据库标识符为空、过长或包含无效字符"));
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

pub(super) fn qualified_table(backend: DbBackend, table: &str) -> Result<String> {
    let schema = match backend {
        DbBackend::Postgres => "public",
        DbBackend::Sqlite => "main",
        _ => return Err(NuonoError::NotImplemented("不支持的数据库后端".into())),
    };
    Ok(format!("\"{schema}\".{}", identifier(table)?))
}

pub(super) fn parameter(backend: DbBackend, index: usize) -> String {
    if backend == DbBackend::Postgres {
        format!("${index}")
    } else {
        "?".into()
    }
}

async fn count_rows(
    conn: &impl ConnectionTrait,
    backend: DbBackend,
    table: &str,
    filter_sql: &str,
    values: Vec<SqlValue>,
) -> Result<u64> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            backend,
            format!("SELECT COUNT(*) AS count FROM {table}{filter_sql}"),
            values,
        ))
        .await?
        .ok_or_else(|| NuonoError::DatabaseQuery("COUNT 查询无结果行".into()))?;
    let count: i64 = row.try_get("", "count")?;
    u64::try_from(count).map_err(|_| NuonoError::DatabaseQuery("数据库返回了负数行数".into()))
}

impl Database {
    /// 开启只读巡检事务并设置事务级超时（`SET LOCAL`），防止慢查询长期占满共享连接池，且超时设置不会泄漏到普通业务查询。
    pub(super) async fn begin_inspection(&self) -> Result<DatabaseTransaction> {
        let tx = if self.backend() == DbBackend::Postgres {
            self.orm()
                .begin_with_config(
                    Some(IsolationLevel::RepeatableRead),
                    Some(AccessMode::ReadOnly),
                )
                .await?
        } else {
            self.orm().begin().await?
        };
        if self.backend() == DbBackend::Postgres {
            tx.execute_unprepared("SET LOCAL statement_timeout = '10s'")
                .await?;
            tx.execute_unprepared("SET LOCAL lock_timeout = '3s'")
                .await?;
        }
        Ok(tx)
    }

    /// 查询数据库占用字节数：PostgreSQL 返回当前库已分配磁盘大小；SQLite 返回主库逻辑页总大小（含当前 WAL 中的页，不含 WAL/SHM 额外文件头开销）。
    pub async fn database_size_bytes(&self) -> Result<u64> {
        let sql = match self.backend() {
            DbBackend::Postgres => "SELECT pg_database_size(current_database()) AS size",
            DbBackend::Sqlite => {
                "SELECT page_count * page_size AS size FROM pragma_page_count(), pragma_page_size()"
            }
            _ => return Err(NuonoError::NotImplemented("不支持的数据库后端".into())),
        };
        let tx = self.begin_inspection().await?;
        let row = tx
            .query_one_raw(Statement::from_string(self.backend(), sql))
            .await?
            .ok_or_else(|| NuonoError::DatabaseQuery("数据库大小查询无结果行".into()))?;
        tx.commit().await?;
        let size: i64 = row.try_get("", "size")?;
        u64::try_from(size).map_err(|_| NuonoError::DatabaseQuery("数据库返回了负数大小".into()))
    }

    async fn inspection_table_names(&self) -> Result<Vec<String>> {
        let sql = match self.backend() {
            DbBackend::Postgres => {
                "SELECT table_name AS name FROM information_schema.tables \
                 WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY table_name"
            }
            DbBackend::Sqlite => {
                "SELECT name FROM pragma_table_list \
                 WHERE schema = 'main' AND type = 'table' AND substr(name, 1, 7) <> 'sqlite_' \
                 ORDER BY name"
            }
            _ => return Err(NuonoError::NotImplemented("不支持的数据库后端".into())),
        };
        let tx = self.begin_inspection().await?;
        let result = tx
            .query_all_raw(Statement::from_string(self.backend(), sql))
            .await?
            .into_iter()
            .map(|row| Ok(row.try_get("", "name")?))
            .collect();
        tx.commit().await?;
        result
    }

    /// 发现应用 schema 中的所有普通业务表（含插件创建的表）；表目录发现阶段只查元数据，绝不扫描表记录统计行数。
    pub async fn inspect_tables(&self) -> Result<Vec<TableInspection>> {
        self.inspect_tables_with_policies(&std::collections::BTreeMap::new())
            .await
    }

    pub async fn inspect_tables_with_policies(
        &self,
        policies: &std::collections::BTreeMap<String, DatabaseTablePolicy>,
    ) -> Result<Vec<TableInspection>> {
        let mut tables = Vec::new();
        for name in self.inspection_table_names().await? {
            let declared = policies
                .get(&name)
                .filter(|_| table_owner(&name) != TableOwner::Core);
            let (browsable, management, restriction) = if let Some(declared) = declared {
                let columns = self
                    .inspection_columns_with_policy(&name, Some(declared))
                    .await?;
                (
                    columns.iter().any(|column| column.readable),
                    policy::management(&name, &columns, Some(declared)),
                    Some("插件声明开放的字段；保存和删除会检查并发修改并记录审计".into()),
                )
            } else {
                (
                    !readable_columns(&name).is_empty(),
                    TableManagement::for_table(&name),
                    restriction(&name),
                )
            };
            tables.push(TableInspection {
                owner: table_owner(&name),
                browsable,
                management,
                restriction,
                name,
                row_count: None,
            });
        }
        Ok(tables)
    }

    pub(super) async fn inspection_columns_with_policy(
        &self,
        table: &str,
        declared: Option<&DatabaseTablePolicy>,
    ) -> Result<Vec<ColumnInspection>> {
        policy::check_policy(table, declared)?;
        identifier(table)?;
        if !self
            .inspection_table_names()
            .await?
            .iter()
            .any(|name| name == table)
        {
            return Err(NuonoError::NotFound(format!("数据库表不存在: {table}")));
        }
        let allowed = declared
            .map(|policy| {
                policy
                    .columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| readable_columns(table).to_vec());
        let (sql, values) = match self.backend() {
            DbBackend::Postgres => (
                "SELECT c.column_name AS name, c.data_type, c.column_default AS default_value, \
                 (c.is_generated <> 'NEVER' OR c.is_identity = 'YES') AS generated, \
                 c.character_maximum_length::BIGINT AS max_length, c.is_nullable = 'YES' AS nullable, \
                 EXISTS (SELECT 1 FROM information_schema.table_constraints t \
                 JOIN information_schema.key_column_usage k ON k.constraint_catalog = t.constraint_catalog \
                 AND k.constraint_schema = t.constraint_schema AND k.constraint_name = t.constraint_name \
                 AND k.table_name = t.table_name \
                 WHERE t.constraint_type = 'PRIMARY KEY' AND t.table_schema = c.table_schema \
                 AND t.table_name = c.table_name AND k.column_name = c.column_name) AS primary_key \
                 FROM information_schema.columns c \
                 WHERE c.table_schema = 'public' AND c.table_name = $1 ORDER BY c.ordinal_position",
                vec![table.into()],
            ),
            DbBackend::Sqlite => (
                "SELECT name, type AS data_type, \"notnull\" = 0 AND NOT (pk > 0 AND upper(type) = 'INTEGER' \
                 AND NOT EXISTS (SELECT 1 FROM pragma_index_list(?, 'main') WHERE origin = 'pk')) \
                 AS nullable, pk > 0 AS primary_key, dflt_value AS default_value, \
                 (hidden > 1 OR (pk > 0 AND upper(type) = 'INTEGER' AND NOT EXISTS \
                 (SELECT 1 FROM pragma_index_list(?, 'main') WHERE origin = 'pk'))) AS generated, \
                 CAST(NULL AS BIGINT) AS max_length FROM pragma_table_xinfo(?, 'main') ORDER BY cid",
                vec![table.into(), table.into(), table.into()],
            ),
            _ => return Err(NuonoError::NotImplemented("不支持的数据库后端".into())),
        };
        let tx = self.begin_inspection().await?;
        let result = tx
            .query_all_raw(Statement::from_sql_and_values(self.backend(), sql, values))
            .await?
            .into_iter()
            .map(|row| {
                let name: String = row.try_get("", "name")?;
                let generated: bool = row.try_get("", "generated")?;
                let primary_key: bool = row.try_get("", "primary_key")?;
                let database_limit: Option<i64> = row.try_get("", "max_length")?;
                let max_length = match (
                    declared
                        .and_then(|policy| {
                            policy
                                .columns
                                .iter()
                                .find(|column| column.name == name)
                                .and_then(|column| column.max_length)
                        })
                        .or_else(|| field_max_length(table, &name)),
                    database_limit.map(|value| value as u64),
                ) {
                    (Some(policy), Some(database)) => Some(policy.min(database)),
                    (policy, database) => policy.or(database),
                };
                let data_type: String = row.try_get("", "data_type")?;
                let edit_type = if !generated {
                    if let Some(declared) = declared {
                        declared
                            .columns
                            .iter()
                            .find(|column| column.name == name)
                            .and_then(|column| column.edit_type)
                            .filter(|kind| {
                                policy::compatible(
                                    *kind,
                                    &data_type,
                                    self.backend() == DbBackend::Sqlite,
                                )
                            })
                            .map(|kind| kind.label().to_owned())
                    } else if !primary_key {
                        edit_type(table, &name, &data_type).map(str::to_owned)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let default_value: Option<String> = row.try_get("", "default_value")?;
                let has_default = default_value.is_some() || generated;
                let readable = allowed.contains(&name.as_str());
                let filter_kind = filters::column_kind(
                    table,
                    &name,
                    &data_type,
                    edit_type.as_deref(),
                    self.backend(),
                );
                let filter_operators = filters::operators(&filter_kind, readable);
                Ok(ColumnInspection {
                    filter_kind,
                    filter_operators,
                    writable: edit_type.is_some() && !primary_key,
                    insertable: edit_type.is_some(),
                    edit_type,
                    has_default,
                    default_value: default_value.filter(|_| allowed.contains(&name.as_str())),
                    generated,
                    max_length,
                    readable: allowed.contains(&name.as_str()),
                    name,
                    data_type,
                    nullable: row.try_get("", "nullable")?,
                    primary_key: row.try_get("", "primary_key")?,
                })
            })
            .collect();
        tx.commit().await?;
        result
    }

    pub async fn inspect_table(&self, table: &str) -> Result<TableSchema> {
        self.inspect_table_with_policy(table, None).await
    }

    pub async fn inspect_table_with_policy(
        &self,
        table: &str,
        declared: Option<&DatabaseTablePolicy>,
    ) -> Result<TableSchema> {
        let columns = self.inspection_columns_with_policy(table, declared).await?;
        Ok(TableSchema {
            table: TableInspection {
                name: table.into(),
                owner: table_owner(table),
                row_count: None,
                browsable: columns.iter().any(|column| column.readable),
                management: policy::management(table, &columns, declared),
                restriction: if declared.is_some() {
                    Some("插件声明开放的字段；保存和删除会检查并发修改并记录审计".into())
                } else {
                    restriction(table)
                },
            },
            columns,
        })
    }

    pub async fn browse_table(&self, table: &str, request: &BrowseRequest) -> Result<BrowsePage> {
        self.browse_table_with_policy(table, request, None).await
    }

    pub async fn browse_table_with_policy(
        &self,
        table: &str,
        request: &BrowseRequest,
        declared: Option<&DatabaseTablePolicy>,
    ) -> Result<BrowsePage> {
        self.query_inspection_page(table, request, MAX_BROWSE_LIMIT, declared)
            .await
    }

    /// 从第一条匹配记录（`offset = 0`）开始导出表数据：复用与分页浏览相同的列白名单策略、筛选条件、排序规则与一致性读快照，但采用独立的导出条数上限。
    pub async fn export_table(&self, table: &str, request: &BrowseRequest) -> Result<BrowsePage> {
        self.export_table_with_policy(table, request, None).await
    }

    pub async fn export_table_with_policy(
        &self,
        table: &str,
        request: &BrowseRequest,
        declared: Option<&DatabaseTablePolicy>,
    ) -> Result<BrowsePage> {
        let request = BrowseRequest {
            offset: 0,
            ..request.clone()
        };
        self.query_inspection_page(table, &request, MAX_EXPORT_LIMIT, declared)
            .await
    }

    async fn query_inspection_page(
        &self,
        table: &str,
        request: &BrowseRequest,
        max_limit: u64,
        declared: Option<&DatabaseTablePolicy>,
    ) -> Result<BrowsePage> {
        if request.limit == 0 || request.limit > max_limit || request.offset > MAX_OFFSET {
            return Err(invalid(format!(
                "每页数量须在 1..={max_limit}，偏移须不大于 {MAX_OFFSET}"
            )));
        }
        filters::validate_request(request)?;
        let schema = self.inspection_columns_with_policy(table, declared).await?;
        let readable: Vec<_> = schema.iter().filter(|column| column.readable).collect();
        if readable.is_empty() {
            return Err(NuonoError::PermissionDenied(
                "此表不开放记录浏览或导出".into(),
            ));
        }
        let resolve_column = |name: &str| -> Result<String> {
            if !readable.iter().any(|column| column.name == name) {
                return Err(invalid("排序或筛选列不存在或未开放读取"));
            }
            identifier(name)
        };
        let (filter_sql, mut values) = filters::build(request, &schema, self.backend())?;
        let mut order = Vec::new();
        if let Some(column) = &request.sort {
            let direction = if request.descending { "DESC" } else { "ASC" };
            order.push(format!("{} {direction}", resolve_column(column)?));
        }
        // 追加主键升序作为排序兜底（tie-breaker），确保所选排序列存在重复值或 NULL 时 OFFSET 分页顺序依然稳定确定。
        for column in &readable {
            if column.primary_key && request.sort.as_deref() != Some(column.name.as_str()) {
                order.push(format!("{} ASC", identifier(&column.name)?));
            }
        }
        if order.is_empty() {
            order.push(format!("{} ASC", identifier(&readable[0].name)?));
        }
        let columns: Vec<String> = readable.iter().map(|column| column.name.clone()).collect();
        let projection = columns
            .iter()
            .map(|name| {
                let name = identifier(name)?;
                Ok(format!("CAST({name} AS TEXT) AS {name}"))
            })
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        let table_sql = qualified_table(self.backend(), table)?;
        // PostgreSQL 默认的 READ COMMITTED 隔离级别是每条语句新建一个快照；
        // begin_inspection 显式开启了 REPEATABLE READ，确保并发写入期间 COUNT 总行数与分页行数据来自同一快照。
        let txn = self.begin_inspection().await?;
        let total = count_rows(
            &txn,
            self.backend(),
            &table_sql,
            &filter_sql,
            values.clone(),
        )
        .await?;
        if max_limit == MAX_EXPORT_LIMIT && total > MAX_EXPORT_LIMIT {
            return Err(invalid(format!(
                "匹配 {total} 条记录，单次最多导出 {MAX_EXPORT_LIMIT} 条，请缩小筛选范围"
            )));
        }
        let limit_parameter = parameter(self.backend(), values.len() + 1);
        let offset_parameter = parameter(self.backend(), values.len() + 2);
        values.push((request.limit as i64).into());
        values.push((request.offset as i64).into());
        let page_sql = format!(
            "FROM {table_sql}{filter_sql} ORDER BY {} LIMIT {limit_parameter} OFFSET {offset_parameter}",
            order.join(", "),
        );
        // 在同一事务快照内先由数据库侧统计本页的总字节数与最大单格字节数，校验通过后再将单元格文本拉入 Rust 内存。
        // 子查询与实际取数采用完全相同的过滤、排序和分页范围：因此即便表中其他行含有超大字段，只要被窄筛选条件或分页排除在本页之外，仍可正常浏览。
        // SQL NULL 记为 0 字节；超限直接报错拒绝，绝不静默截断。
        let lengths = columns
            .iter()
            .map(|column| {
                let column = identifier(column)?;
                Ok(if self.backend() == DbBackend::Postgres {
                    format!("CAST(COALESCE(octet_length(CAST({column} AS TEXT)), 0) AS BIGINT)")
                } else {
                    format!("COALESCE(length(CAST(CAST({column} AS TEXT) AS BLOB)), 0)")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let largest = if lengths.len() == 1 {
            lengths[0].clone()
        } else {
            let function = if self.backend() == DbBackend::Postgres {
                "GREATEST"
            } else {
                "MAX"
            };
            format!("{function}({})", lengths.join(", "))
        };
        let byte_query = format!(
            "SELECT CAST(COALESCE(SUM(page_bytes), 0) AS BIGINT) AS total_bytes, \
             CAST(COALESCE(MAX(cell_bytes), 0) AS BIGINT) AS largest_cell_bytes \
             FROM (SELECT ({}) AS page_bytes, {largest} AS cell_bytes {page_sql}) AS inspected_page",
            lengths.join(" + "),
        );
        let sizes = txn
            .query_one_raw(Statement::from_sql_and_values(
                self.backend(),
                byte_query,
                values.clone(),
            ))
            .await?
            .ok_or_else(|| NuonoError::DatabaseQuery("数据库记录大小查询无结果行".into()))?;
        let total_bytes: i64 = sizes.try_get("", "total_bytes")?;
        let largest_cell_bytes: i64 = sizes.try_get("", "largest_cell_bytes")?;
        if total_bytes > MAX_PAGE_BYTES || largest_cell_bytes > MAX_CELL_BYTES {
            return Err(invalid(
                "查询结果超过大小限制：单个字段最多 1 MiB，总内容最多 16 MiB；请缩小筛选范围或每页数量",
            ));
        }
        let sql = format!("SELECT {projection} {page_sql}",);
        let result = txn
            .query_all_raw(Statement::from_sql_and_values(self.backend(), sql, values))
            .await?;
        let mut rows = Vec::with_capacity(result.len());
        for row in result {
            let mut object = Map::new();
            for column in &columns {
                let value: Option<String> = row.try_get("", column)?;
                object.insert(
                    column.clone(),
                    value.map(Value::String).unwrap_or(Value::Null),
                );
            }
            rows.push(Value::Object(object));
        }
        txn.commit().await?;
        Ok(BrowsePage {
            table: table.into(),
            columns,
            rows,
            total,
            offset: request.offset,
            limit: request.limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;

    #[tokio::test]
    async fn inspection_limits_are_local_and_cancel_slow_queries() {
        let db = test_db(1).await;
        let tx = db.begin_inspection().await.unwrap();
        if db.backend() == DbBackend::Postgres {
            let row = tx
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SHOW statement_timeout",
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                row.try_get::<String>("", "statement_timeout").unwrap(),
                "10s"
            );
            tx.execute_unprepared("SET LOCAL statement_timeout = '30ms'")
                .await
                .unwrap();
            assert!(tx.execute_unprepared("SELECT pg_sleep(0.2)").await.is_err());
            tx.rollback().await.unwrap();
            let row = db
                .orm()
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SHOW statement_timeout",
                ))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(
                row.try_get::<String>("", "statement_timeout").unwrap(),
                "30ms"
            );
        } else {
            tx.commit().await.unwrap();
        }
        assert!(!db.inspect_tables().await.unwrap().is_empty());
        db.shutdown().await.unwrap();
    }

    async fn execute(db: &Database, sql: &str, values: Vec<SqlValue>) {
        db.orm()
            .execute_raw(Statement::from_sql_and_values(db.backend(), sql, values))
            .await
            .unwrap();
    }

    async fn seed_filter_rules(db: &Database) {
        for (pattern, target_id, filter_type) in [
            ("ordinary", "Zed", "same"),
            ("contains %_ ' OR 1=1 --", "Ada", "same"),
            ("另一条消息", "Ming", "other"),
        ] {
            let placeholders = (1..=3)
                .map(|index| parameter(db.backend(), index))
                .collect::<Vec<_>>()
                .join(", ");
            execute(
                db,
                &format!(
                    "INSERT INTO nuono_filter_rules (pattern, target_id, filter_type) VALUES ({placeholders})"
                ),
                vec![pattern.into(), target_id.into(), filter_type.into()],
            )
            .await;
        }
    }

    #[tokio::test]
    async fn inspection_discovers_real_tables_and_schema() {
        let db = test_db(1).await;
        let core = db.inspect_tables().await.unwrap();
        assert!(!core.is_empty());
        assert!(core.iter().all(|table| table.owner == TableOwner::Core));
        assert_eq!(
            db.inspect_table("nuono_users").await.unwrap().table.owner,
            TableOwner::Core
        );
        execute(&db, "CREATE TABLE plugin_inspection_fixture (id INTEGER PRIMARY KEY, label TEXT NOT NULL, note TEXT)", vec![]).await;
        execute(
            &db,
            "CREATE VIEW inspection_fixture_view AS SELECT id FROM plugin_inspection_fixture",
            vec![],
        )
        .await;
        execute(
            &db,
            "INSERT INTO plugin_inspection_fixture (id, label) VALUES (1, 'plugin row')",
            vec![],
        )
        .await;
        if db.backend() == DbBackend::Postgres {
            execute(&db, "CREATE SCHEMA inspection_other", vec![]).await;
            execute(
                &db,
                "CREATE TABLE inspection_other.hidden_table (id INTEGER)",
                vec![],
            )
            .await;
        } else {
            execute(
                &db,
                "CREATE VIRTUAL TABLE inspection_search USING fts5(target)",
                vec![],
            )
            .await;
            // 主 schema 发现不得读到同名临时表。
            execute(
                &db,
                "CREATE TEMP TABLE plugin_inspection_fixture (wrong TEXT)",
                vec![],
            )
            .await;
        }
        let tables = db.inspect_tables().await.unwrap();
        let table = tables
            .iter()
            .find(|table| table.name == "plugin_inspection_fixture")
            .unwrap();
        assert_eq!(table.row_count, None);
        assert!(tables.iter().all(|table| table.row_count.is_none()));
        assert_eq!(table.owner, TableOwner::Unassigned);
        assert!(!table.browsable);
        assert!(table.restriction.is_some());
        assert!(
            !tables
                .iter()
                .any(|table| table.name == "inspection_fixture_view"
                    || table.name == "hidden_table"
                    || table.name.starts_with("inspection_search")
                    || table.name.starts_with("sqlite_"))
        );
        let schema = db.inspect_table("plugin_inspection_fixture").await.unwrap();
        assert_eq!(schema.table.owner, TableOwner::Unassigned);
        assert_eq!(schema.columns.len(), 3);
        assert!(schema.columns[0].primary_key);
        assert!(!schema.columns[0].nullable);
        assert!(!schema.columns[1].nullable);
        assert!(schema.columns[2].nullable);
        assert!(schema.columns.iter().all(|column| !column.readable));
        // SQLite 允许普通复合主键列包含 NULL，这与隐式作为非空 rowid 别名的单列 INTEGER PRIMARY KEY 不同。
        execute(&db, "CREATE TABLE inspection_composite (first INTEGER, second INTEGER, PRIMARY KEY (first, second))", vec![]).await;
        let composite = db.inspect_table("inspection_composite").await.unwrap();
        assert!(composite.columns.iter().all(|column| column.primary_key));
        assert!(
            composite
                .columns
                .iter()
                .all(|column| column.nullable == (db.backend() == DbBackend::Sqlite))
        );
        assert!(db.database_size_bytes().await.unwrap() > 0);
        assert!(matches!(
            db.browse_table("plugin_inspection_fixture", &BrowseRequest::default())
                .await,
            Err(NuonoError::PermissionDenied(_))
        ));
        assert!(matches!(
            db.inspect_table("does_not_exist").await,
            Err(NuonoError::NotFound(_))
        ));
        assert!(
            db.inspect_table("nuono_users\"; DROP TABLE nuono_users; --")
                .await
                .is_err()
        );
        assert!(db.inspect_table("nuono_users").await.is_ok());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn inspection_browse_filters_paginates_and_exports() {
        let db = test_db(2).await;
        seed_filter_rules(&db).await;
        let page = db
            .browse_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    limit: 1,
                    offset: 1,
                    sort: Some("target_id".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0]["target_id"], "Ming");
        let filtered_request = BrowseRequest {
            filter_column: Some("pattern".into()),
            filter_value: Some("%_ ' OR 1=1 --".into()),
            sort: Some("id".into()),
            descending: true,
            ..Default::default()
        };
        let filtered = db
            .browse_table("nuono_filter_rules", &filtered_request)
            .await
            .unwrap();
        assert_eq!(filtered.total, 1);
        assert_eq!(filtered.rows[0]["target_id"], "Ada");
        let exported = db
            .export_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    offset: 100,
                    ..filtered_request
                },
            )
            .await
            .unwrap();
        assert_eq!(exported.offset, 0);
        assert_eq!(exported.rows, filtered.rows);
        let ties = db
            .browse_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    sort: Some("filter_type".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(ties.rows[0]["target_id"], "Ming");
        assert_eq!(ties.rows[1]["target_id"], "Zed");
        assert_eq!(ties.rows[2]["target_id"], "Ada");
        // 值保持为字符串；不得经 f64 转换以免丢精度。
        execute(
            &db,
            "INSERT INTO nuono_filter_rules (id, pattern, target_id, filter_type, created_at) VALUES (9007199254740993, 'large integer', 'fixture', 'fixture', NULL)",
            vec![],
        )
        .await;
        let numbers = db
            .browse_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    sort: Some("id".into()),
                    descending: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(numbers.rows[0]["id"], "9007199254740993");
        assert_eq!(numbers.rows[0]["created_at"], Value::Null);
        assert!(
            db.browse_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    limit: MAX_BROWSE_LIMIT + 1,
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        assert!(
            db.export_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    limit: MAX_BROWSE_LIMIT + 1,
                    ..Default::default()
                }
            )
            .await
            .is_ok()
        );
        assert!(
            db.export_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    limit: MAX_EXPORT_LIMIT + 1,
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        for request in [
            BrowseRequest {
                limit: 0,
                ..Default::default()
            },
            BrowseRequest {
                offset: MAX_OFFSET + 1,
                ..Default::default()
            },
            BrowseRequest {
                filter_column: Some("pattern".into()),
                ..Default::default()
            },
            BrowseRequest {
                filter_value: Some("value".into()),
                ..Default::default()
            },
            BrowseRequest {
                filter_column: Some("pattern".into()),
                filter_value: Some("a".repeat(MAX_FILTER_BYTES + 1)),
                ..Default::default()
            },
            BrowseRequest {
                sort: Some("id; DROP TABLE nuono_users".into()),
                ..Default::default()
            },
        ] {
            assert!(
                db.browse_table("nuono_filter_rules", &request)
                    .await
                    .is_err()
            );
        }
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn inspection_rejects_oversized_cells_before_loading_and_allows_narrow_filter() {
        let db = test_db(2).await;
        seed_filter_rules(&db).await;
        let sql = format!(
            "INSERT INTO nuono_filter_rules (pattern, target_id, filter_type) VALUES ({}, 'Oversized', 'large')",
            parameter(db.backend(), 1)
        );
        execute(
            &db,
            &sql,
            vec!["大".repeat(MAX_CELL_BYTES as usize / 3 + 1).into()],
        )
        .await;
        for result in [
            db.browse_table("nuono_filter_rules", &BrowseRequest::default())
                .await,
            db.export_table("nuono_filter_rules", &BrowseRequest::default())
                .await,
        ] {
            assert!(
                matches!(result, Err(NuonoError::InvalidInput(message)) if message.contains("大小限制"))
            );
        }
        let narrow = BrowseRequest {
            filter_column: Some("target_id".into()),
            filter_value: Some("Ada".into()),
            ..Default::default()
        };
        assert_eq!(
            db.browse_table("nuono_filter_rules", &narrow)
                .await
                .unwrap()
                .rows
                .len(),
            1
        );
        assert_eq!(
            db.export_table("nuono_filter_rules", &narrow)
                .await
                .unwrap()
                .rows
                .len(),
            1
        );
        // 仅取第 1 条记录的首页查询不应受后续行中超大单元格的影响。
        assert_eq!(
            db.browse_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    limit: 1,
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .rows
            .len(),
            1
        );
        execute(
            &db,
            "DELETE FROM nuono_filter_rules WHERE target_id = 'Oversized'",
            vec![],
        )
        .await;
        // 即使每个单元格单独都在 1 MiB 限制内，整页累计字节数也必须遵守 16 MiB 上限。
        let pattern = "a".repeat(MAX_CELL_BYTES as usize - 100);
        for _ in 0..17 {
            execute(&db, &sql, vec![pattern.clone().into()]).await;
        }
        assert!(
            matches!(db.export_table("nuono_filter_rules", &BrowseRequest::default()).await,
            Err(NuonoError::InvalidInput(message)) if message.contains("大小限制"))
        );
        assert!(
            db.browse_table(
                "nuono_filter_rules",
                &BrowseRequest {
                    limit: 5,
                    ..Default::default()
                }
            )
            .await
            .is_ok()
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn inspection_never_selects_or_filters_credentials() {
        let db = test_db(2).await;
        db.ensure_ai_module().await.unwrap();
        execute(&db, "INSERT INTO nuono_users (username, username_key, password_hash, totp_secret, totp_recovery_json, created_at_ms, updated_at_ms) VALUES ('inspector', 'inspector', 'PASSWORD_MARKER', $1, $2, 0, 0)", vec![b"TOTP_MARKER".to_vec().into(),serde_json::json!(["RECOVERY_MARKER"]).to_string().into()]).await;
        execute(&db, "INSERT INTO nuono_ai_models (name, provider, model_id, api_key, base_url) VALUES ('test', 'test', 'test', 'API_KEY_MARKER', 'https://user:URL_SECRET@example.com')", vec![]).await;
        execute(
            &db,
            "ALTER TABLE nuono_users ADD COLUMN inspection_new_secret TEXT",
            vec![],
        )
        .await;
        execute(
            &db,
            "UPDATE nuono_users SET inspection_new_secret = 'NEW_SECRET_MARKER'",
            vec![],
        )
        .await;
        db.set_secret("inspection.secret", "RUNTIME_SECRET_MARKER")
            .await
            .unwrap();
        for table in [
            "nuono_ai_personas",
            "nuono_account_assistant",
            "nuono_companion_memory_sources",
            "nuono_assistant_turns",
            "nuono_agent_runs",
            "nuono_agent_memories",
            "nuono_agent_memory_graphs",
            "nuono_conversations",
            "nuono_ai_runs",
            "nuono_runtime_secrets",
            "nuono_sessions",
            "nuono_account_challenges",
            "nuono_account_merge_jobs",
            "nuono_global_configs",
            "nuono_plugin_kv",
        ] {
            assert!(
                !db.inspect_table(table).await.unwrap().table.browsable,
                "{table}"
            );
            assert!(
                matches!(
                    db.browse_table(table, &BrowseRequest::default()).await,
                    Err(NuonoError::PermissionDenied(_))
                ),
                "{table}"
            );
            assert!(
                matches!(
                    db.export_table(table, &BrowseRequest::default()).await,
                    Err(NuonoError::PermissionDenied(_))
                ),
                "{table}"
            );
        }
        let schema = db.inspect_table("nuono_users").await.unwrap();
        for column in [
            "password_hash",
            "totp_secret",
            "totp_recovery_json",
            "inspection_new_secret",
        ] {
            assert!(
                !schema
                    .columns
                    .iter()
                    .find(|item| item.name == column)
                    .unwrap()
                    .readable
            );
            assert!(
                db.browse_table(
                    "nuono_users",
                    &BrowseRequest {
                        sort: Some(column.into()),
                        ..Default::default()
                    }
                )
                .await
                .is_err()
            );
            assert!(
                db.browse_table(
                    "nuono_users",
                    &BrowseRequest {
                        filter_column: Some(column.into()),
                        filter_value: Some("MARKER".into()),
                        ..Default::default()
                    }
                )
                .await
                .is_err()
            );
        }
        let page = db
            .browse_table("nuono_users", &BrowseRequest::default())
            .await
            .unwrap();
        assert_eq!(page.rows[0]["username"], "inspector");
        assert!(!serde_json::to_string(&page).unwrap().contains("MARKER"));
        assert!(!page.columns.contains(&"password_hash".into()));
        let export = db
            .export_table("nuono_users", &BrowseRequest::default())
            .await
            .unwrap();
        assert_eq!(page.rows, export.rows);
        let models = db
            .browse_table("nuono_ai_models", &BrowseRequest::default())
            .await
            .unwrap();
        assert!(!serde_json::to_string(&models).unwrap().contains("SECRET"));
        assert!(!serde_json::to_string(&models).unwrap().contains("MARKER"));
        db.shutdown().await.unwrap();
    }
}
