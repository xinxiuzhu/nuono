//! 插件持久化数据访问接口（共享 `sea_orm::Statement` 等查询描述类型，底层数据库连接与驱动由宿主存储层持有）。
use nuono_error::Result;
use serde_json::Value;

mod state;
pub use state::{
    PluginStateCollection, PluginStateRecord, PluginStateScope, PluginStateStamp, PluginStateValue,
};
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginMigration {
    /// 插件作用域内的迁移版本号，必须 `>= 1`。
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub version: i64,
    /// 迁移说明，非空白。
    pub description: String,
    /// 迁移 SQL（PostgreSQL 方言），非空白；建议为幂等 DDL。
    pub sql: String,
}

/// 一条双后端 Native 插件业务 schema 迁移：同一版本号分别给出两种方言的幂等 SQL。
///
/// 见 `run_plugin_migrations_for_backend`；版本号语义与 [`PluginMigration`]
/// 完全一致（插件名作用域，记录同样写进 `plugin_migrations`）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginMigrationForBackend {
    /// 插件作用域内的迁移版本号，必须 `>= 1`；同一次调用内不可重复。
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub version: i64,
    /// 迁移说明，非空白。
    pub description: String,
    /// PostgreSQL 方言 SQL，非空白；建议为幂等 DDL。
    pub pg_sql: String,
    /// SQLite 方言 SQL，非空白；建议为幂等 DDL。
    pub sqlite_sql: String,
}

impl PluginMigrationForBackend {
    /// 纯声明校验；两个方言均须完整提供，不连接数据库、不执行 SQL。
    pub fn validate_all(migrations: &[Self]) -> Result<()> {
        let mut versions = std::collections::HashSet::new();
        for migration in migrations {
            let version = migration.version;
            let invalid = if version <= 0 {
                Some("版本必须 >= 1")
            } else if !versions.insert(version) {
                Some("版本在同一次声明中重复")
            } else if migration.description.trim().is_empty() {
                Some("描述不能为空")
            } else if migration.pg_sql.trim().is_empty() {
                Some("PostgreSQL SQL 不能为空")
            } else if migration.sqlite_sql.trim().is_empty() {
                Some("SQLite SQL 不能为空")
            } else {
                None
            };
            if let Some(reason) = invalid {
                return Err(nuono_error::NuonoError::InvalidInput(format!(
                    "插件迁移 {version}: {reason}"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum PluginDatabaseBackend {
    Postgres,
    Sqlite,
}

/// 将统一编号参数占位符（`$1`, `$2`, ...）按目标数据库方言改写。
///
/// - PostgreSQL：原样返回，零分配；
/// - SQLite：跳过单引号字符串、双引号标识符与 SQL 注释，仅将 `$N`（`N >= 1`）改写为 `?N`。
///   已有的 `?` / `?N` 占位符保持不变。
pub fn rewrite_sql_placeholders(
    sql: &str,
    backend: PluginDatabaseBackend,
) -> std::borrow::Cow<'_, str> {
    if backend != PluginDatabaseBackend::Sqlite || !sql.contains('$') {
        return std::borrow::Cow::Borrowed(sql);
    }
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;
    let mut last_copy = 0;

    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';

    while i < len {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < len {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < len && bytes[i] == b'\'' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < len {
                    if bytes[i] == b'"' {
                        i += 1;
                        if i < len && bytes[i] == b'"' {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < len && bytes[i + 1] == b'-' => {
                i += 2;
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(len);
            }
            b'$' if (i == 0 || !is_ident_byte(bytes[i - 1]))
                && i + 1 < len
                && matches!(bytes[i + 1], b'1'..=b'9') =>
            {
                let mut end = i + 2;
                while end < len && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end == len || !is_ident_byte(bytes[end]) {
                    out.push_str(&sql[last_copy..i]);
                    out.push('?');
                    out.push_str(&sql[i + 1..end]);
                    last_copy = end;
                    i = end;
                } else {
                    i = end;
                }
            }
            _ => i += 1,
        }
    }

    if last_copy == 0 {
        std::borrow::Cow::Borrowed(sql)
    } else {
        out.push_str(&sql[last_copy..]);
        std::borrow::Cow::Owned(out)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum PluginSqlValue {
    /// NULL 文本参数。其他 NULL 变体保留 PostgreSQL 参数类型。
    Null,
    NullBool,
    NullInteger,
    NullReal,
    NullBytes,
    Bool(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Bytes(Vec<u8>),
}
/// 插件数据库事务接口（由底层存储实现持有，未显式调用 `commit` 即丢弃时会自动回滚）。
#[async_trait::async_trait]
pub trait PluginDataTransaction: Send + Sync {
    async fn configuration_resource(
        &self,
        _key: &str,
    ) -> Result<Option<crate::configuration_resources::ConfigurationResource>> {
        Err(state_unavailable())
    }
    async fn save_configuration_resource(
        &self,
        _write: &crate::configuration_resources::ValidatedConfigurationResource,
    ) -> Result<crate::configuration_resources::ConfigurationResource> {
        Err(state_unavailable())
    }

    async fn require_administrator(
        &self,
        _access: &crate::plugin_identity::IdentityAccess,
    ) -> Result<()> {
        Err(state_unavailable())
    }
    async fn prepare_group_configuration(
        &self,
        _access: &crate::plugin_identity::IdentityAccess,
        _group: &crate::plugin_identity::CanonicalGroup,
    ) -> Result<crate::configuration::GroupConfigurationCandidates> {
        Err(state_unavailable())
    }
    async fn require_group_configuration(
        &self,
        _access: &crate::plugin_identity::IdentityAccess,
        _group: &crate::plugin_identity::CanonicalGroup,
        _proof: Option<crate::configuration::PlatformRoleProof>,
    ) -> Result<()> {
        Err(state_unavailable())
    }

    async fn state_record(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
    ) -> Result<Option<PluginStateRecord>> {
        Err(state_unavailable())
    }
    async fn state_create(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
        _value_version: i32,
        _value: Value,
        _expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        Err(state_unavailable())
    }
    async fn state_compare_exchange(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
        _expected: &PluginStateStamp,
        _value_version: i32,
        _value: Value,
        _expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        Err(state_unavailable())
    }
    async fn state_delete_if(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
        _expected: &PluginStateStamp,
    ) -> Result<bool> {
        Err(state_unavailable())
    }
    async fn state_prune_expired(
        &self,
        _collection: &PluginStateCollection,
        _limit: u32,
    ) -> Result<u64> {
        Err(state_unavailable())
    }

    async fn identity_access_by_id(
        &self,
        _id: i64,
    ) -> Result<crate::plugin_identity::IdentityAccess> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未提供平台身份解析".into(),
        ))
    }

    fn backend(&self) -> PluginDatabaseBackend;

    async fn require_identity(
        &self,
        _access: &crate::plugin_identity::IdentityAccess,
    ) -> Result<()> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未提供身份版本检查".into(),
        ))
    }
    async fn require_group(&self, _group: &crate::plugin_identity::CanonicalGroup) -> Result<()> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未提供群版本检查".into(),
        ))
    }

    /// 在锁定插件业务表数据行前，先按用户 ID 升序批量获取账号锁以防死锁。
    async fn lock_accounts(&self, _user_ids: &[i64]) -> Result<()> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未绑定授权能力".into(),
        ))
    }
    async fn group_grants(
        &self,
        _group_id: i64,
    ) -> Result<Vec<crate::group_authorization::GroupAuthorization>> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未绑定授权签发插件".into(),
        ))
    }
    async fn issue_group_grant(
        &self,
        _request: &crate::group_authorization::GroupGrantRequest,
    ) -> Result<crate::group_authorization::GroupAuthorization> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未绑定授权签发插件".into(),
        ))
    }
    async fn renew_group_grant(
        &self,
        _id: i64,
        _revision: i64,
        _expires_at_ms: Option<i64>,
    ) -> Result<crate::group_authorization::GroupAuthorization> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未绑定授权签发插件".into(),
        ))
    }
    async fn revoke_group_grant(
        &self,
        _id: i64,
        _revision: i64,
    ) -> Result<crate::group_authorization::GroupAuthorization> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未绑定授权签发插件".into(),
        ))
    }

    /// 在业务事务内读取共享状态。
    async fn state_get(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
    ) -> Result<Option<PluginStateValue>> {
        Err(nuono_error::NuonoError::DatabaseQuery(
            "transactional state is not supported by this store".into(),
        ))
    }

    /// 在业务事务内写入共享状态，沿用版本和大小限制。
    async fn state_set(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
        _value_version: i32,
        _value_json: String,
    ) -> Result<()> {
        Err(nuono_error::NuonoError::DatabaseQuery(
            "transactional state is not supported by this store".into(),
        ))
    }

    /// 锁定有效账号并读取个人助手授权。持锁到事务结束，续期前先调用此方法。
    async fn assistant_grant(
        &self,
        _user_id: i64,
    ) -> Result<Option<crate::assistant::AssistantGrant>> {
        Err(nuono_error::NuonoError::DatabaseQuery(
            "assistant grants are not supported by this store".into(),
        ))
    }

    /// 与插件业务数据原子更新助手授权。None 撤销授权，不清除当日用量或对话。
    async fn set_assistant_grant(
        &self,
        _user_id: i64,
        _grant: Option<crate::assistant::AssistantGrant>,
    ) -> Result<()> {
        Err(nuono_error::NuonoError::DatabaseQuery(
            "assistant grants are not supported by this store".into(),
        ))
    }

    /// 在当前事务内幂等写入或刷新开源功能权益记录。
    async fn upsert_entitlement(
        &self,
        _input: &crate::entitlement::EntitlementUpsert,
    ) -> Result<crate::entitlement::EntitlementRecord> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未绑定权益写入能力".into(),
        ))
    }

    /// 在当前事务内撤销指定来源的功能权益记录。
    async fn revoke_entitlement(
        &self,
        _subject_type: &str,
        _subject_id: i64,
        _feature_key: &str,
        _source: &str,
    ) -> Result<bool> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "事务未绑定权益撤销能力".into(),
        ))
    }

    /// 在当前事务连接与隔离作用域内读取插件机密配置。
    async fn get_secret(&self, _key: &str) -> Result<Option<String>> {
        Err(nuono_error::NuonoError::DatabaseQuery(
            "transactional plugin secret reads are not supported by this store".into(),
        ))
    }

    /// 在与插件业务记录相同的数据库事务内写入插件机密配置（传入空字符串表示删除该密钥，与常规写入语义一致）。
    async fn set_secret(&self, _key: &str, _value: &str) -> Result<()> {
        Err(nuono_error::NuonoError::DatabaseQuery(
            "transactional plugin secrets are not supported by this store".into(),
        ))
    }

    /// 在宿主连接上执行 ORM 语句，保留原生值类型。
    async fn orm_execute(
        &self,
        _statement: sea_orm::Statement,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        Err(sea_orm::DbErr::Custom(
            "ORM execution is unavailable in this store".into(),
        ))
    }
    async fn orm_query(
        &self,
        _statement: sea_orm::Statement,
    ) -> std::result::Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
        Err(sea_orm::DbErr::Custom(
            "ORM queries are unavailable in this store".into(),
        ))
    }
    async fn orm_execute_unprepared(
        &self,
        _sql: &str,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        Err(sea_orm::DbErr::Custom(
            "ORM schema execution is unavailable in this store".into(),
        ))
    }

    async fn execute(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<u64>;
    async fn query_json(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<Vec<Value>>;
    /// 执行普通 SELECT 查询，将每行各列直接映射为 JSON 对象（无需手工 `json_object` / `json_build_object`）。
    async fn query_rows(&self, _sql: &str, _values: Vec<PluginSqlValue>) -> Result<Vec<Value>> {
        Err(state_unavailable())
    }
    async fn commit(self: Box<Self>) -> Result<()>;
    async fn rollback(self: Box<Self>) -> Result<()>;
}

#[async_trait::async_trait]
pub trait PluginDataStore: Send + Sync {
    async fn is_administrator(
        &self,
        _access: &crate::plugin_identity::IdentityAccess,
    ) -> Result<bool> {
        Err(state_unavailable())
    }

    async fn state_record(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
    ) -> Result<Option<PluginStateRecord>> {
        Err(state_unavailable())
    }
    async fn state_create(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
        _value_version: i32,
        _value: Value,
        _expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        Err(state_unavailable())
    }
    async fn state_compare_exchange(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
        _expected: &PluginStateStamp,
        _value_version: i32,
        _value: Value,
        _expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        Err(state_unavailable())
    }
    async fn state_delete_if(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
        _expected: &PluginStateStamp,
    ) -> Result<bool> {
        Err(state_unavailable())
    }
    async fn state_prune_expired(
        &self,
        _collection: &PluginStateCollection,
        _limit: u32,
    ) -> Result<u64> {
        Err(state_unavailable())
    }

    fn backend(&self) -> PluginDatabaseBackend;

    async fn resolve_identity(
        &self,
        _identity: &nuono_domain::IdentityRef,
    ) -> Result<crate::plugin_identity::IdentityAccess> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "宿主未提供平台身份解析".into(),
        ))
    }
    async fn identity_access_by_id(
        &self,
        _id: i64,
    ) -> Result<crate::plugin_identity::IdentityAccess> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "宿主未提供平台身份解析".into(),
        ))
    }
    async fn find_group(
        &self,
        _conversation: &nuono_domain::ConversationRef,
    ) -> Result<Option<crate::plugin_identity::CanonicalGroup>> {
        Err(nuono_error::NuonoError::NotImplemented(
            "宿主未提供只读群查询".into(),
        ))
    }
    async fn resolve_group(
        &self,
        _conversation: &nuono_domain::ConversationRef,
    ) -> Result<Option<crate::plugin_identity::CanonicalGroup>> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "宿主未提供群对象解析".into(),
        ))
    }
    async fn require_identity(
        &self,
        _access: &crate::plugin_identity::IdentityAccess,
    ) -> Result<()> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "宿主未提供身份版本检查".into(),
        ))
    }

    /// 开启绑定到指定插件 ID 的数据库事务（事务内固定持有该插件身份，后续写入调用无法篡改）。
    async fn begin_for_plugin(&self, _plugin_id: &str) -> Result<Box<dyn PluginDataTransaction>> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "存储不支持插件签发身份".into(),
        ))
    }
    async fn require_group(&self, _group: &crate::plugin_identity::CanonicalGroup) -> Result<()> {
        Err(nuono_error::NuonoError::PermissionDenied(
            "宿主未提供群身份核验".into(),
        ))
    }

    /// 查询指定主体在当前时刻是否拥有任意来源的有效功能权益。
    async fn has_active_entitlement(
        &self,
        _subject_type: &str,
        _subject_id: i64,
        _feature_key: &str,
    ) -> Result<bool> {
        Err(nuono_error::NuonoError::NotImplemented(
            "宿主未提供功能权益查询".into(),
        ))
    }

    /// 在宿主连接上执行 ORM 语句，保留原生值类型。
    async fn orm_execute(
        &self,
        _statement: sea_orm::Statement,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        Err(sea_orm::DbErr::Custom(
            "ORM execution is unavailable in this store".into(),
        ))
    }
    async fn orm_query(
        &self,
        _statement: sea_orm::Statement,
    ) -> std::result::Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
        Err(sea_orm::DbErr::Custom(
            "ORM queries are unavailable in this store".into(),
        ))
    }
    async fn orm_execute_unprepared(
        &self,
        _sql: &str,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        Err(sea_orm::DbErr::Custom(
            "ORM schema execution is unavailable in this store".into(),
        ))
    }

    async fn begin(&self) -> Result<Box<dyn PluginDataTransaction>> {
        Err(nuono_error::NuonoError::DatabaseQuery(
            "plugin transactions are not supported by this store".into(),
        ))
    }
    async fn get_secret(&self, key: &str) -> Result<Option<String>>;
    async fn set_secret(&self, key: &str, value: &str) -> Result<()>;
    async fn delete_secret(&self, key: &str) -> Result<()>;
    async fn kv_get(&self, namespace: &str, key: &str) -> Result<Option<String>>;
    async fn kv_set(&self, namespace: &str, key: &str, value: &str) -> Result<()>;
    async fn kv_delete(&self, namespace: &str, key: &str) -> Result<bool>;
    async fn kv_list(&self, namespace: &str) -> Result<Vec<(String, String)>>;
    async fn kv_clear(&self, namespace: &str) -> Result<u64>;
    /// 读取宿主绑定的集合内的一条状态。
    async fn state_get(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
    ) -> Result<Option<PluginStateValue>> {
        Err(state_unavailable())
    }
    /// 原子替换宿主绑定的集合内的一条状态。
    async fn state_set(
        &self,
        _collection: &PluginStateCollection,
        _key: &str,
        _value_version: i32,
        _value_json: String,
    ) -> Result<()> {
        Err(state_unavailable())
    }
    /// 删除宿主绑定的集合内的一条状态。
    async fn state_delete(&self, _collection: &PluginStateCollection, _key: &str) -> Result<bool> {
        Err(state_unavailable())
    }
    /// 按键升序读取一页状态，`after` 是不包含在结果里的上一页末尾键。
    async fn state_list(
        &self,
        _collection: &PluginStateCollection,
        _after: Option<&str>,
        _limit: u32,
    ) -> Result<Vec<(String, PluginStateValue)>> {
        Err(state_unavailable())
    }
    async fn migrate(&self, namespace: &str, migrations: &[PluginMigration]) -> Result<()>;
    async fn migrate_backends(
        &self,
        namespace: &str,
        migrations: &[PluginMigrationForBackend],
    ) -> Result<()>;
    async fn execute(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<u64>;
    /// 执行返回单列 `data` JSON 文本的查询（需使用当前数据库方言的 JSON 构造函数，在 PostgreSQL 上需将 JSON 转为文本）。
    async fn query_json(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<Vec<Value>>;
    /// 执行普通 SELECT 查询，将每行各列直接映射为 JSON 对象（无需手工 `json_object` / `json_build_object`）。
    async fn query_rows(&self, _sql: &str, _values: Vec<PluginSqlValue>) -> Result<Vec<Value>> {
        Err(state_unavailable())
    }
}

fn state_unavailable() -> nuono_error::NuonoError {
    nuono_error::NuonoError::NotImplemented("此插件仓储未提供共享状态".into())
}

#[cfg(test)]
mod migration_declaration_tests {
    use super::*;

    fn migration() -> PluginMigrationForBackend {
        PluginMigrationForBackend {
            version: 1,
            description: "create notes".into(),
            pg_sql: "CREATE TABLE notes (id TEXT PRIMARY KEY)".into(),
            sqlite_sql: "CREATE TABLE notes (id TEXT PRIMARY KEY)".into(),
        }
    }

    #[test]
    fn both_dialects_and_unique_positive_versions_are_required_before_execution() {
        assert!(PluginMigrationForBackend::validate_all(&[]).is_ok());
        let valid = migration();
        let mut next = valid.clone();
        next.version = 2;
        assert!(PluginMigrationForBackend::validate_all(&[next, valid.clone()]).is_ok());
        assert!(PluginMigrationForBackend::validate_all(&[valid.clone(), valid]).is_err());
        for invalid in [
            PluginMigrationForBackend {
                version: 0,
                ..migration()
            },
            PluginMigrationForBackend {
                version: -1,
                ..migration()
            },
            PluginMigrationForBackend {
                description: " ".into(),
                ..migration()
            },
            PluginMigrationForBackend {
                pg_sql: " ".into(),
                ..migration()
            },
            PluginMigrationForBackend {
                sqlite_sql: " ".into(),
                ..migration()
            },
        ] {
            assert!(PluginMigrationForBackend::validate_all(&[invalid]).is_err());
        }
    }

    #[test]
    fn sql_placeholders_rewrite_only_outside_literals_and_comments() {
        let sql = "SELECT id, '$1' AS lit, \"$2\" AS col -- $3\n/* $4 */ FROM t WHERE a = $1 AND b = $10 AND c = $1";
        assert_eq!(
            rewrite_sql_placeholders(sql, PluginDatabaseBackend::Postgres).as_ref(),
            sql
        );
        assert_eq!(
            rewrite_sql_placeholders(sql, PluginDatabaseBackend::Sqlite).as_ref(),
            "SELECT id, '$1' AS lit, \"$2\" AS col -- $3\n/* $4 */ FROM t WHERE a = ?1 AND b = ?10 AND c = ?1"
        );
        let escaped = "SELECT 'it''s $1' FROM t WHERE x = $1";
        assert_eq!(
            rewrite_sql_placeholders(escaped, PluginDatabaseBackend::Sqlite).as_ref(),
            "SELECT 'it''s $1' FROM t WHERE x = ?1"
        );
    }
}
