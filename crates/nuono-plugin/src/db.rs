//! 由宿主托管的插件持久化数据库接口，支持 SeaORM 实体查询与事务操作。
pub use nuono_app::plugin_data::{
    PluginDataStore, PluginDataTransaction, PluginDatabaseBackend, PluginMigration,
    PluginMigrationForBackend, PluginSqlValue, PluginStateCollection, PluginStateRecord,
    PluginStateScope, PluginStateStamp, PluginStateValue, rewrite_sql_placeholders,
};
pub use nuono_domain::database::{DatabaseColumnPolicy, DatabaseFieldType, DatabaseTablePolicy};
pub mod config_resources;
mod orm;
pub struct Transaction {
    inner: Arc<tokio::sync::Mutex<Option<Box<dyn PluginDataTransaction>>>>,
    resources_changed: std::sync::atomic::AtomicBool,
    runtime: Option<crate::runtime::RuntimeExecutor>,
    backend: PluginDatabaseBackend,
    plugin_id: Option<String>,
    configuration: Option<Arc<dyn nuono_app::configuration::PluginConfigurationAccess>>,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("backend", &self.backend)
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

use nuono_error::Result;
use std::sync::Arc;
#[derive(Clone)]
pub struct Database {
    store: Arc<dyn PluginDataStore>,
    runtime: Option<crate::runtime::RuntimeExecutor>,
    plugin_id: Option<String>,
    configuration: Option<Arc<dyn nuono_app::configuration::PluginConfigurationAccess>>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("plugin_id", &self.plugin_id)
            .field("has_runtime", &self.runtime.is_some())
            .finish_non_exhaustive()
    }
}
impl Database {
    pub async fn is_administrator(&self, access: &crate::identity::IdentityAccess) -> Result<bool> {
        let store = self.store.clone();
        let access = access.clone();
        self.run(async move { store.is_administrator(&access).await })
            .await
    }

    fn require_collection(&self, collection: &PluginStateCollection) -> Result<()> {
        if self
            .plugin_id
            .as_deref()
            .is_some_and(|id| id != collection.plugin_id())
        {
            return Err(nuono_error::NuonoError::PermissionDenied(
                "状态集合不属于当前插件".into(),
            ));
        }
        Ok(())
    }
    pub async fn state_record(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateRecord>> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        let key = key.to_owned();
        let store = self.store.clone();
        self.run(async move { store.state_record(&collection, &key).await })
            .await
    }
    pub async fn state_create(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        let key = key.to_owned();
        let store = self.store.clone();
        self.run(async move {
            store
                .state_create(&collection, &key, value_version, value, expires_at_ms)
                .await
        })
        .await
    }
    pub async fn state_compare_exchange(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        let key = key.to_owned();
        let expected = expected.clone();
        let store = self.store.clone();
        self.run(async move {
            store
                .state_compare_exchange(
                    &collection,
                    &key,
                    &expected,
                    value_version,
                    value,
                    expires_at_ms,
                )
                .await
        })
        .await
    }
    pub async fn state_delete_if(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
    ) -> Result<bool> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        let key = key.to_owned();
        let expected = expected.clone();
        let store = self.store.clone();
        self.run(async move { store.state_delete_if(&collection, &key, &expected).await })
            .await
    }
    pub async fn state_prune_expired(
        &self,
        collection: &PluginStateCollection,
        limit: u32,
    ) -> Result<u64> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        let store = self.store.clone();
        self.run(async move { store.state_prune_expired(&collection, limit).await })
            .await
    }

    pub async fn find_group(
        &self,
        conversation: &nuono_domain::ConversationRef,
    ) -> Result<Option<crate::identity::CanonicalGroup>> {
        let store = self.store.clone();
        let conversation = conversation.clone();
        self.run(async move { store.find_group(&conversation).await })
            .await
    }

    pub async fn resolve_identity(
        &self,
        identity: &nuono_domain::IdentityRef,
    ) -> Result<crate::identity::IdentityAccess> {
        let store = self.store.clone();
        let identity = identity.clone();
        self.run(async move { store.resolve_identity(&identity).await })
            .await
    }
    pub async fn identity_access_by_id(&self, id: i64) -> Result<crate::identity::IdentityAccess> {
        let store = self.store.clone();
        self.run(async move { store.identity_access_by_id(id).await })
            .await
    }
    pub async fn resolve_group(
        &self,
        conversation: &nuono_domain::ConversationRef,
    ) -> Result<Option<crate::identity::CanonicalGroup>> {
        let store = self.store.clone();
        let conversation = conversation.clone();
        self.run(async move { store.resolve_group(&conversation).await })
            .await
    }
    pub async fn require_identity(&self, access: &crate::identity::IdentityAccess) -> Result<()> {
        let store = self.store.clone();
        let access = access.clone();
        self.run(async move { store.require_identity(&access).await })
            .await
    }

    pub async fn require_group(&self, group: &crate::identity::CanonicalGroup) -> Result<()> {
        let store = self.store.clone();
        let group = group.clone();
        self.run(async move { store.require_group(&group).await })
            .await
    }

    /// 查询指定主体当前是否拥有任意有效来源（计费、人工或试用）的对应功能权益。
    pub async fn has_active_entitlement(
        &self,
        subject_type: &str,
        subject_id: i64,
        feature_key: &str,
    ) -> Result<bool> {
        let store = self.store.clone();
        let subject_type = subject_type.to_owned();
        let feature_key = feature_key.to_owned();
        self.run(async move {
            store
                .has_active_entitlement(&subject_type, subject_id, &feature_key)
                .await
        })
        .await
    }
    pub fn new(store: impl PluginDataStore + 'static) -> Self {
        Self {
            store: Arc::new(store),
            runtime: crate::runtime::RuntimeExecutor::current(),
            plugin_id: None,
            configuration: None,
        }
    }
    /// 供宿主构造时使用：将已安装插件的稳定标识（`plugin_id`）绑定到其发起的所有事务上。
    pub fn for_plugin(store: impl PluginDataStore + 'static, plugin_id: impl Into<String>) -> Self {
        Self {
            plugin_id: Some(plugin_id.into()),
            ..Self::new(store)
        }
    }
    /// 供宿主注入绑定到当前已安装插件作用域的配置访问服务。
    pub fn with_configuration_access(
        mut self,
        access: Arc<dyn nuono_app::configuration::PluginConfigurationAccess>,
    ) -> Self {
        self.configuration = Some(access);
        self
    }
    /// 供后台定时/异步任务只读获取当前插件在指定群聊下的生效配置。
    pub async fn group_configuration_for_task(
        &self,
        group: &crate::identity::CanonicalGroup,
    ) -> Result<nuono_app::configuration::PluginConfigurationSnapshot> {
        let service = self.configuration.clone().ok_or_else(|| {
            nuono_error::NuonoError::PermissionDenied("宿主未提供群配置服务".into())
        })?;
        let group = group.clone();
        self.run(async move { service.group_configuration_for_task(&group).await })
            .await
    }
    pub async fn group_configuration(
        &self,
        actor: &crate::identity::IdentityAccess,
        group: &crate::identity::CanonicalGroup,
    ) -> Result<nuono_app::configuration::PluginConfigurationSnapshot> {
        let service = self.configuration.clone().ok_or_else(|| {
            nuono_error::NuonoError::PermissionDenied("宿主未提供群配置服务".into())
        })?;
        let (actor, group) = (actor.clone(), group.clone());
        self.run(async move { service.group_configuration(&actor, &group).await })
            .await
    }
    pub async fn save_group_configuration(
        &self,
        actor: &crate::identity::IdentityAccess,
        group: &crate::identity::CanonicalGroup,
        patch: nuono_app::configuration::OverridePatch,
    ) -> Result<nuono_app::configuration::PluginConfigurationSnapshot> {
        let service = self.configuration.clone().ok_or_else(|| {
            nuono_error::NuonoError::PermissionDenied("宿主未提供群配置服务".into())
        })?;
        let (actor, group) = (actor.clone(), group.clone());
        self.run(async move {
            service
                .save_group_configuration(&actor, &group, patch)
                .await
        })
        .await
    }
    pub fn backend(&self) -> PluginDatabaseBackend {
        self.store.backend()
    }
    async fn run<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        match &self.runtime {
            Some(runtime) => runtime.run(future).await,
            None => future.await,
        }
    }
    /// 开启一个原子写入事务（必须显式调用 `commit` 提交；若提前返回或被丢弃则自动回滚）。
    /// 在 SQLite 后端下，宿主会在首次读取前直接获取写锁（`BEGIN IMMEDIATE`），避免后续读写升级死锁。
    pub async fn begin(&self) -> Result<Transaction> {
        let store = self.store.clone();
        let runtime = self.runtime.clone();
        let plugin_id = self.plugin_id.clone();
        let configuration = self.configuration.clone();
        self.run(async move {
            let inner = match &plugin_id {
                Some(id) => store.begin_for_plugin(id).await?,
                None => store.begin().await?,
            };
            let mut transaction = Transaction::new(inner, runtime);
            transaction.plugin_id = plugin_id;
            transaction.configuration = configuration;
            Ok(transaction)
        })
        .await
    }
    pub async fn get_secret(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_owned();
        let store = self.store.clone();
        self.run(async move { store.get_secret(&key).await }).await
    }
    pub async fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        let key = key.to_owned();
        let value = value.to_owned();
        let store = self.store.clone();
        self.run(async move { store.set_secret(&key, &value).await })
            .await
    }
    pub async fn delete_secret(&self, key: &str) -> Result<()> {
        let key = key.to_owned();
        let store = self.store.clone();
        self.run(async move { store.delete_secret(&key).await })
            .await
    }
    pub async fn kv_get(&self, namespace: &str, key: &str) -> Result<Option<String>> {
        let namespace = namespace.to_owned();
        let key = key.to_owned();
        let store = self.store.clone();
        self.run(async move { store.kv_get(&namespace, &key).await })
            .await
    }
    pub async fn kv_set(&self, namespace: &str, key: &str, value: &str) -> Result<()> {
        let namespace = namespace.to_owned();
        let key = key.to_owned();
        let value = value.to_owned();
        let store = self.store.clone();
        self.run(async move { store.kv_set(&namespace, &key, &value).await })
            .await
    }
    pub async fn kv_delete(&self, namespace: &str, key: &str) -> Result<bool> {
        let namespace = namespace.to_owned();
        let key = key.to_owned();
        let store = self.store.clone();
        self.run(async move { store.kv_delete(&namespace, &key).await })
            .await
    }
    pub async fn kv_list(&self, namespace: &str) -> Result<Vec<(String, String)>> {
        let namespace = namespace.to_owned();
        let store = self.store.clone();
        self.run(async move { store.kv_list(&namespace).await })
            .await
    }
    pub async fn kv_clear(&self, namespace: &str) -> Result<u64> {
        let namespace = namespace.to_owned();
        let store = self.store.clone();
        self.run(async move { store.kv_clear(&namespace).await })
            .await
    }
    pub(crate) async fn state_get(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateValue>> {
        let (collection, key) = (collection.clone(), key.to_owned());
        let store = self.store.clone();
        self.run(async move { store.state_get(&collection, &key).await })
            .await
    }
    pub(crate) async fn state_set(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value_json: String,
    ) -> Result<()> {
        let (collection, key) = (collection.clone(), key.to_owned());
        let store = self.store.clone();
        self.run(async move {
            store
                .state_set(&collection, &key, value_version, value_json)
                .await
        })
        .await
    }
    pub(crate) async fn state_delete(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<bool> {
        let (collection, key) = (collection.clone(), key.to_owned());
        let store = self.store.clone();
        self.run(async move { store.state_delete(&collection, &key).await })
            .await
    }
    /// 按记录键升序读取集合的一页，after 使用上一页最后一个键。
    ///
    /// # Errors
    /// 游标无效、limit 超出 1–100 或存储查询失败时返回错误。
    pub async fn state_list(
        &self,
        collection: &PluginStateCollection,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<(String, PluginStateValue)>> {
        let (collection, after) = (collection.clone(), after.map(str::to_owned));
        let store = self.store.clone();
        self.run(async move { store.state_list(&collection, after.as_deref(), limit).await })
            .await
    }
    pub async fn run_plugin_migrations(
        &self,
        namespace: &str,
        migrations: &[PluginMigration],
    ) -> Result<()> {
        let namespace = namespace.to_owned();
        let migrations = migrations.to_vec();
        let store = self.store.clone();
        self.run(async move { store.migrate(&namespace, &migrations).await })
            .await
    }
    /// 显式迁移入口，供独立测试/导入工具使用。插件生命周期请用 `Plugin::migrations()` 声明。
    pub async fn run_plugin_migrations_for_backend(
        &self,
        namespace: &str,
        migrations: &[PluginMigrationForBackend],
    ) -> Result<()> {
        let namespace = namespace.to_owned();
        let migrations = migrations.to_vec();
        let store = self.store.clone();
        self.run(async move { store.migrate_backends(&namespace, &migrations).await })
            .await
    }
    pub async fn execute(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<u64> {
        let sql = sql.to_owned();
        let store = self.store.clone();
        self.run(async move { store.execute(&sql, values).await })
            .await
    }
    pub async fn query_json(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        let sql = sql.to_owned();
        let store = self.store.clone();
        self.run(async move { store.query_json(&sql, values).await })
            .await
    }

    /// 执行普通 SELECT 查询并返回每行的 JSON 对象（支持统一的 `$1, $2` 占位符，自动适配 SQLite/PostgreSQL）。
    pub async fn query_rows(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        let sql = sql.to_owned();
        let store = self.store.clone();
        self.run(async move { store.query_rows(&sql, values).await })
            .await
    }

    /// 执行 SELECT 查询并将每行反序列化为强类型结构体 `T`（支持统一的 `$1, $2` 占位符）。
    pub async fn query_as<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<T>> {
        let rows = self.query_rows(sql, values).await?;
        rows.into_iter()
            .map(|row| {
                serde_json::from_value(row)
                    .map_err(|error| nuono_error::NuonoError::Deserialization(error.to_string()))
            })
            .collect()
    }

    /// 执行 SELECT 查询并反序列化首行记录；无结果时返回 `Ok(None)`。
    pub async fn query_optional_as<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Option<T>> {
        let Some(row) = self.query_rows(sql, values).await?.into_iter().next() else {
            return Ok(None);
        };
        serde_json::from_value(row)
            .map(Some)
            .map_err(|error| nuono_error::NuonoError::Deserialization(error.to_string()))
    }

    /// 执行单列标量查询，返回首行首列的值；无结果或列值为 `NULL` 时返回 `Ok(None)`。
    pub async fn query_scalar<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Option<T>> {
        extract_scalar(self.query_rows(sql, values).await?)
    }
}

fn extract_scalar<T: serde::de::DeserializeOwned>(
    rows: Vec<serde_json::Value>,
) -> Result<Option<T>> {
    let Some(first) = rows.into_iter().next() else {
        return Ok(None);
    };
    let value = match first {
        serde_json::Value::Object(map) => match map.into_values().next() {
            Some(value) => value,
            None => return Ok(None),
        },
        other => other,
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|error| nuono_error::NuonoError::Deserialization(error.to_string()))
}
#[async_trait::async_trait]
impl PluginDataStore for Database {
    async fn is_administrator(&self, access: &crate::identity::IdentityAccess) -> Result<bool> {
        Database::is_administrator(self, access).await
    }

    async fn state_record(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateRecord>> {
        Database::state_record(self, collection, key).await
    }
    async fn state_create(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        Database::state_create(self, collection, key, value_version, value, expires_at_ms).await
    }
    async fn state_compare_exchange(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        Database::state_compare_exchange(
            self,
            collection,
            key,
            expected,
            value_version,
            value,
            expires_at_ms,
        )
        .await
    }
    async fn state_delete_if(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
    ) -> Result<bool> {
        Database::state_delete_if(self, collection, key, expected).await
    }
    async fn state_prune_expired(
        &self,
        collection: &PluginStateCollection,
        limit: u32,
    ) -> Result<u64> {
        Database::state_prune_expired(self, collection, limit).await
    }

    async fn find_group(
        &self,
        conversation: &nuono_domain::ConversationRef,
    ) -> Result<Option<crate::identity::CanonicalGroup>> {
        Database::find_group(self, conversation).await
    }

    async fn require_group(&self, group: &crate::identity::CanonicalGroup) -> Result<()> {
        Database::require_group(self, group).await
    }

    async fn resolve_identity(
        &self,
        identity: &nuono_domain::IdentityRef,
    ) -> Result<crate::identity::IdentityAccess> {
        Database::resolve_identity(self, identity).await
    }
    async fn identity_access_by_id(&self, id: i64) -> Result<crate::identity::IdentityAccess> {
        Database::identity_access_by_id(self, id).await
    }
    async fn resolve_group(
        &self,
        conversation: &nuono_domain::ConversationRef,
    ) -> Result<Option<crate::identity::CanonicalGroup>> {
        Database::resolve_group(self, conversation).await
    }
    async fn require_identity(&self, access: &crate::identity::IdentityAccess) -> Result<()> {
        Database::require_identity(self, access).await
    }

    async fn orm_execute(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        let store = self.store.clone();
        self.run(async move { Ok(store.orm_execute(statement).await) })
            .await
            .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?
    }
    async fn orm_query(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
        let store = self.store.clone();
        self.run(async move { Ok(store.orm_query(statement).await) })
            .await
            .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?
    }
    async fn orm_execute_unprepared(
        &self,
        sql: &str,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        let store = self.store.clone();
        let sql = sql.to_owned();
        self.run(async move { Ok(store.orm_execute_unprepared(&sql).await) })
            .await
            .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?
    }

    fn backend(&self) -> PluginDatabaseBackend {
        self.backend()
    }
    async fn begin(&self) -> Result<Box<dyn PluginDataTransaction>> {
        Ok(Box::new(self.begin().await?))
    }
    async fn get_secret(&self, key: &str) -> Result<Option<String>> {
        self.get_secret(key).await
    }
    async fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        self.set_secret(key, value).await
    }
    async fn delete_secret(&self, key: &str) -> Result<()> {
        self.delete_secret(key).await
    }
    async fn kv_get(&self, namespace: &str, key: &str) -> Result<Option<String>> {
        self.kv_get(namespace, key).await
    }
    async fn kv_set(&self, namespace: &str, key: &str, value: &str) -> Result<()> {
        self.kv_set(namespace, key, value).await
    }
    async fn kv_delete(&self, namespace: &str, key: &str) -> Result<bool> {
        self.kv_delete(namespace, key).await
    }
    async fn kv_list(&self, namespace: &str) -> Result<Vec<(String, String)>> {
        self.kv_list(namespace).await
    }
    async fn kv_clear(&self, namespace: &str) -> Result<u64> {
        self.kv_clear(namespace).await
    }
    async fn migrate(&self, namespace: &str, migrations: &[PluginMigration]) -> Result<()> {
        self.run_plugin_migrations(namespace, migrations).await
    }
    async fn migrate_backends(
        &self,
        namespace: &str,
        migrations: &[PluginMigrationForBackend],
    ) -> Result<()> {
        self.run_plugin_migrations_for_backend(namespace, migrations)
            .await
    }
    async fn execute(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<u64> {
        self.execute(sql, values).await
    }
    async fn query_json(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        self.query_json(sql, values).await
    }
    async fn query_rows(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        self.query_rows(sql, values).await
    }
}

impl Transaction {
    pub async fn configuration_resource(
        &self,
        key: &str,
    ) -> Result<Option<nuono_app::configuration_resources::ConfigurationResource>> {
        let key = key.to_owned();
        self.run(move |tx| Box::pin(async move { tx.configuration_resource(&key).await }))
            .await
    }
    pub async fn save_configuration_resource(
        &self,
        write: &nuono_app::configuration_resources::ValidatedConfigurationResource,
    ) -> Result<nuono_app::configuration_resources::ConfigurationResource> {
        let write = write.clone();
        let saved = self
            .run(move |tx| Box::pin(async move { tx.save_configuration_resource(&write).await }))
            .await?;
        self.resources_changed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(saved)
    }

    pub async fn require_administrator(
        &self,
        access: &crate::identity::IdentityAccess,
    ) -> Result<()> {
        let access = access.clone();
        self.run(move |tx| Box::pin(async move { tx.require_administrator(&access).await }))
            .await
    }
    /// 锁定并校验当前操作者对该群的配置修改权限（结合平台实时群角色核验），并在事务提交前再次复核。
    pub async fn require_group_configuration(
        &self,
        access: &crate::identity::IdentityAccess,
        group: &crate::identity::CanonicalGroup,
    ) -> Result<()> {
        let (owned_access, owned_group) = (access.clone(), group.clone());
        let candidates = self
            .run(move |tx| {
                Box::pin(async move {
                    tx.prepare_group_configuration(&owned_access, &owned_group)
                        .await
                })
            })
            .await?;
        let proof = if let Some(service) = self.configuration.clone() {
            self.run_future(async move { service.verify_group_role(candidates).await })
                .await?
        } else {
            None
        };
        let (access, group) = (access.clone(), group.clone());
        self.run(move |tx| {
            Box::pin(async move { tx.require_group_configuration(&access, &group, proof).await })
        })
        .await
    }

    fn require_collection(&self, collection: &PluginStateCollection) -> Result<()> {
        if self
            .plugin_id
            .as_deref()
            .is_some_and(|id| id != collection.plugin_id())
        {
            return Err(nuono_error::NuonoError::PermissionDenied(
                "状态集合不属于当前插件".into(),
            ));
        }
        Ok(())
    }
    pub async fn state_record(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateRecord>> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        let key = key.to_owned();
        self.run(move |tx| Box::pin(async move { tx.state_record(&collection, &key).await }))
            .await
    }
    pub async fn state_create(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        let key = key.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                tx.state_create(&collection, &key, value_version, value, expires_at_ms)
                    .await
            })
        })
        .await
    }
    pub async fn state_compare_exchange(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        let key = key.to_owned();
        let expected = expected.clone();
        self.run(move |tx| {
            Box::pin(async move {
                tx.state_compare_exchange(
                    &collection,
                    &key,
                    &expected,
                    value_version,
                    value,
                    expires_at_ms,
                )
                .await
            })
        })
        .await
    }
    pub async fn state_delete_if(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
    ) -> Result<bool> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        let key = key.to_owned();
        let expected = expected.clone();
        self.run(move |tx| {
            Box::pin(async move { tx.state_delete_if(&collection, &key, &expected).await })
        })
        .await
    }
    pub async fn state_prune_expired(
        &self,
        collection: &PluginStateCollection,
        limit: u32,
    ) -> Result<u64> {
        self.require_collection(collection)?;
        let collection = collection.clone();
        self.run(move |tx| {
            Box::pin(async move { tx.state_prune_expired(&collection, limit).await })
        })
        .await
    }

    pub async fn identity_access_by_id(&self, id: i64) -> Result<crate::identity::IdentityAccess> {
        self.run(move |tx| Box::pin(async move { tx.identity_access_by_id(id).await }))
            .await
    }

    pub async fn require_identity(&self, access: &crate::identity::IdentityAccess) -> Result<()> {
        let access = access.clone();
        self.run(move |tx| Box::pin(async move { tx.require_identity(&access).await }))
            .await
    }
    pub async fn require_group(&self, group: &crate::identity::CanonicalGroup) -> Result<()> {
        let group = group.clone();
        self.run(move |tx| Box::pin(async move { tx.require_group(&group).await }))
            .await
    }

    pub async fn lock_accounts(&self, user_ids: &[i64]) -> Result<()> {
        let user_ids = user_ids.to_vec();
        self.run(move |tx| Box::pin(async move { tx.lock_accounts(&user_ids).await }))
            .await
    }
    pub async fn group_grants(
        &self,
        group_id: i64,
    ) -> Result<Vec<nuono_app::group_authorization::GroupAuthorization>> {
        self.run(move |tx| Box::pin(async move { tx.group_grants(group_id).await }))
            .await
    }
    pub async fn issue_group_grant(
        &self,
        request: &nuono_app::group_authorization::GroupGrantRequest,
    ) -> Result<nuono_app::group_authorization::GroupAuthorization> {
        let request = request.clone();
        self.run(move |tx| Box::pin(async move { tx.issue_group_grant(&request).await }))
            .await
    }
    pub async fn renew_group_grant(
        &self,
        id: i64,
        revision: i64,
        expires_at_ms: Option<i64>,
    ) -> Result<nuono_app::group_authorization::GroupAuthorization> {
        self.run(move |tx| {
            Box::pin(async move { tx.renew_group_grant(id, revision, expires_at_ms).await })
        })
        .await
    }
    pub async fn revoke_group_grant(
        &self,
        id: i64,
        revision: i64,
    ) -> Result<nuono_app::group_authorization::GroupAuthorization> {
        self.run(move |tx| Box::pin(async move { tx.revoke_group_grant(id, revision).await }))
            .await
    }
    /// 在当前事务内读取已绑定集合的状态。
    ///
    /// # Errors
    /// 键无效、事务已关闭或存储查询失败时返回错误。
    pub async fn state_get(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateValue>> {
        let (collection, key) = (collection.clone(), key.to_owned());
        self.run(move |tx| Box::pin(async move { tx.state_get(&collection, &key).await }))
            .await
    }
    /// 在当前事务内写入共享状态，和业务变更一起提交或回滚。
    ///
    /// # Errors
    /// 键、版本或 JSON 无效、超过 64 KiB，或事务写入失败时返回错误。
    pub async fn state_set(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value_json: String,
    ) -> Result<()> {
        let (collection, key) = (collection.clone(), key.to_owned());
        self.run(move |tx| {
            Box::pin(async move {
                tx.state_set(&collection, &key, value_version, value_json)
                    .await
            })
        })
        .await
    }

    /// 锁定账号并读取助手授权，供同一事务内续期、调整或撤销。
    ///
    /// # Errors
    /// 账号不存在、已禁用，或事务读取失败时返回错误。
    pub async fn assistant_grant(
        &self,
        user_id: i64,
    ) -> Result<Option<crate::assistant::AssistantGrant>> {
        self.run(move |tx| Box::pin(async move { tx.assistant_grant(user_id).await }))
            .await
    }

    /// 在当前事务中写入助手授权，保留聊天、绑定与当日用量。
    ///
    /// 仍需调用 commit 才会提交。None 表示撤销已有授权。
    ///
    /// # Errors
    /// 账号不可用、额度超出范围或事务写入失败时返回错误。
    pub async fn set_assistant_grant(
        &self,
        user_id: i64,
        grant: Option<crate::assistant::AssistantGrant>,
    ) -> Result<()> {
        self.run(move |tx| Box::pin(async move { tx.set_assistant_grant(user_id, grant).await }))
            .await
    }

    /// 在当前事务内幂等写入或刷新开源功能权益记录。
    pub async fn upsert_entitlement(
        &self,
        input: &nuono_app::entitlement::EntitlementUpsert,
    ) -> Result<nuono_app::entitlement::EntitlementRecord> {
        let input = input.clone();
        self.run(move |tx| Box::pin(async move { tx.upsert_entitlement(&input).await }))
            .await
    }

    /// 在当前事务内撤销指定来源的功能权益记录。
    pub async fn revoke_entitlement(
        &self,
        subject_type: &str,
        subject_id: i64,
        feature_key: &str,
        source: &str,
    ) -> Result<bool> {
        let subject_type = subject_type.to_owned();
        let feature_key = feature_key.to_owned();
        let source = source.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                tx.revoke_entitlement(&subject_type, subject_id, &feature_key, &source)
                    .await
            })
        })
        .await
    }

    fn new(
        inner: Box<dyn PluginDataTransaction>,
        runtime: Option<crate::runtime::RuntimeExecutor>,
    ) -> Self {
        let backend = inner.backend();
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(Some(inner))),
            resources_changed: std::sync::atomic::AtomicBool::new(false),
            runtime,
            backend,
            plugin_id: None,
            configuration: None,
        }
    }

    async fn run_future<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        match &self.runtime {
            Some(runtime) => runtime.run(future).await,
            None => future.await,
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl for<'a> FnOnce(
            &'a dyn PluginDataTransaction,
        ) -> futures::future::BoxFuture<'a, Result<T>>
        + Send
        + 'static,
    ) -> Result<T> {
        let inner = self.inner.clone();
        self.run_future(async move {
            // 通过异步互斥锁串行访问事务连接：若前一次宿主操作已被取消，
            // 后续操作或提交/回滚会在此等待前一次操作彻底释放底层连接后再继续。
            let guard = inner.lock().await;
            let transaction = guard.as_ref().ok_or_else(|| {
                nuono_error::NuonoError::DatabaseQuery(
                    "plugin transaction is already closed".into(),
                )
            })?;
            operation(transaction.as_ref()).await
        })
        .await
    }

    /// 通过当前事务所在的数据库连接读取一条标准化的插件私有密钥。
    /// 插件业务代码通常应优先调用 `PluginSecretStore::get_private_in_transaction`。
    pub async fn get_secret(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_owned();
        self.run(move |transaction| Box::pin(async move { transaction.get_secret(&key).await }))
            .await
    }

    pub(crate) async fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        let key = key.to_owned();
        let value = value.to_owned();
        self.run(move |transaction| {
            Box::pin(async move { transaction.set_secret(&key, &value).await })
        })
        .await
    }

    async fn finish(self, commit: bool) -> Result<()> {
        let inner = self.inner.clone();
        self.run_future(async move {
            let transaction = inner.lock().await.take().ok_or_else(|| {
                nuono_error::NuonoError::DatabaseQuery(
                    "plugin transaction is already closed".into(),
                )
            })?;
            // 用 `take()` 移走底层事务句柄的所有权，防止在提交/回滚被取消后还能再次重复提交。
            if commit {
                transaction.commit().await
            } else {
                transaction.rollback().await
            }
        })
        .await
    }

    pub async fn commit(self) -> Result<()> {
        let changed = self
            .resources_changed
            .load(std::sync::atomic::Ordering::Acquire);
        let configuration = self.configuration.clone();
        self.finish(true).await?;
        if changed && let Some(configuration) = configuration {
            configuration.configuration_resources_committed().await?;
        }
        Ok(())
    }
    pub async fn rollback(self) -> Result<()> {
        self.finish(false).await
    }

    pub async fn execute(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<u64> {
        let sql = sql.to_owned();
        self.run(move |transaction| {
            Box::pin(async move { transaction.execute(&sql, values).await })
        })
        .await
    }

    pub async fn query_json(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        let sql = sql.to_owned();
        self.run(move |transaction| {
            Box::pin(async move { transaction.query_json(&sql, values).await })
        })
        .await
    }

    /// 在当前事务内执行普通 SELECT 查询并返回每行的 JSON 对象（自动适配 `$1, $2` 占位符）。
    pub async fn query_rows(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        let sql = sql.to_owned();
        self.run(move |transaction| {
            Box::pin(async move { transaction.query_rows(&sql, values).await })
        })
        .await
    }

    /// 在当前事务内执行 SELECT 查询并反序列化为强类型列表 `Vec<T>`。
    pub async fn query_as<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<T>> {
        let rows = self.query_rows(sql, values).await?;
        rows.into_iter()
            .map(|row| {
                serde_json::from_value(row)
                    .map_err(|error| nuono_error::NuonoError::Deserialization(error.to_string()))
            })
            .collect()
    }

    /// 在当前事务内执行 SELECT 查询并反序列化首行记录。
    pub async fn query_optional_as<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Option<T>> {
        let Some(row) = self.query_rows(sql, values).await?.into_iter().next() else {
            return Ok(None);
        };
        serde_json::from_value(row)
            .map(Some)
            .map_err(|error| nuono_error::NuonoError::Deserialization(error.to_string()))
    }

    /// 在当前事务内执行单列标量查询，无结果或 `NULL` 时返回 `Ok(None)`。
    pub async fn query_scalar<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Option<T>> {
        extract_scalar(self.query_rows(sql, values).await?)
    }
}

#[async_trait::async_trait]
impl PluginDataTransaction for Transaction {
    async fn configuration_resource(
        &self,
        key: &str,
    ) -> Result<Option<nuono_app::configuration_resources::ConfigurationResource>> {
        Transaction::configuration_resource(self, key).await
    }
    async fn save_configuration_resource(
        &self,
        write: &nuono_app::configuration_resources::ValidatedConfigurationResource,
    ) -> Result<nuono_app::configuration_resources::ConfigurationResource> {
        Transaction::save_configuration_resource(self, write).await
    }

    async fn require_administrator(&self, access: &crate::identity::IdentityAccess) -> Result<()> {
        Transaction::require_administrator(self, access).await
    }
    async fn prepare_group_configuration(
        &self,
        access: &crate::identity::IdentityAccess,
        group: &crate::identity::CanonicalGroup,
    ) -> Result<nuono_app::configuration::GroupConfigurationCandidates> {
        let (access, group) = (access.clone(), group.clone());
        self.run(move |tx| {
            Box::pin(async move { tx.prepare_group_configuration(&access, &group).await })
        })
        .await
    }
    async fn require_group_configuration(
        &self,
        access: &crate::identity::IdentityAccess,
        group: &crate::identity::CanonicalGroup,
        proof: Option<nuono_app::configuration::PlatformRoleProof>,
    ) -> Result<()> {
        let (access, group) = (access.clone(), group.clone());
        self.run(move |tx| {
            Box::pin(async move { tx.require_group_configuration(&access, &group, proof).await })
        })
        .await
    }

    async fn state_record(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateRecord>> {
        Transaction::state_record(self, collection, key).await
    }
    async fn state_create(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        Transaction::state_create(self, collection, key, value_version, value, expires_at_ms).await
    }
    async fn state_compare_exchange(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
        value_version: i32,
        value: serde_json::Value,
        expires_at_ms: Option<i64>,
    ) -> Result<Option<PluginStateRecord>> {
        Transaction::state_compare_exchange(
            self,
            collection,
            key,
            expected,
            value_version,
            value,
            expires_at_ms,
        )
        .await
    }
    async fn state_delete_if(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        expected: &PluginStateStamp,
    ) -> Result<bool> {
        Transaction::state_delete_if(self, collection, key, expected).await
    }
    async fn state_prune_expired(
        &self,
        collection: &PluginStateCollection,
        limit: u32,
    ) -> Result<u64> {
        Transaction::state_prune_expired(self, collection, limit).await
    }

    async fn identity_access_by_id(&self, id: i64) -> Result<crate::identity::IdentityAccess> {
        Transaction::identity_access_by_id(self, id).await
    }
    async fn require_identity(&self, access: &crate::identity::IdentityAccess) -> Result<()> {
        Transaction::require_identity(self, access).await
    }
    async fn require_group(&self, group: &crate::identity::CanonicalGroup) -> Result<()> {
        Transaction::require_group(self, group).await
    }

    async fn lock_accounts(&self, user_ids: &[i64]) -> Result<()> {
        self.lock_accounts(user_ids).await
    }
    async fn group_grants(
        &self,
        group_id: i64,
    ) -> Result<Vec<nuono_app::group_authorization::GroupAuthorization>> {
        self.group_grants(group_id).await
    }
    async fn issue_group_grant(
        &self,
        request: &nuono_app::group_authorization::GroupGrantRequest,
    ) -> Result<nuono_app::group_authorization::GroupAuthorization> {
        self.issue_group_grant(request).await
    }
    async fn renew_group_grant(
        &self,
        id: i64,
        revision: i64,
        expires_at_ms: Option<i64>,
    ) -> Result<nuono_app::group_authorization::GroupAuthorization> {
        self.renew_group_grant(id, revision, expires_at_ms).await
    }
    async fn revoke_group_grant(
        &self,
        id: i64,
        revision: i64,
    ) -> Result<nuono_app::group_authorization::GroupAuthorization> {
        self.revoke_group_grant(id, revision).await
    }
    async fn state_get(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> Result<Option<PluginStateValue>> {
        Transaction::state_get(self, collection, key).await
    }
    async fn state_set(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value_json: String,
    ) -> Result<()> {
        Transaction::state_set(self, collection, key, value_version, value_json).await
    }

    async fn assistant_grant(
        &self,
        user_id: i64,
    ) -> Result<Option<crate::assistant::AssistantGrant>> {
        Transaction::assistant_grant(self, user_id).await
    }
    async fn set_assistant_grant(
        &self,
        user_id: i64,
        grant: Option<crate::assistant::AssistantGrant>,
    ) -> Result<()> {
        Transaction::set_assistant_grant(self, user_id, grant).await
    }

    fn backend(&self) -> PluginDatabaseBackend {
        self.backend
    }
    async fn get_secret(&self, key: &str) -> Result<Option<String>> {
        Transaction::get_secret(self, key).await
    }
    async fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        Transaction::set_secret(self, key, value).await
    }
    async fn execute(&self, sql: &str, values: Vec<PluginSqlValue>) -> Result<u64> {
        Transaction::execute(self, sql, values).await
    }
    async fn query_json(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        Transaction::query_json(self, sql, values).await
    }
    async fn query_rows(
        &self,
        sql: &str,
        values: Vec<PluginSqlValue>,
    ) -> Result<Vec<serde_json::Value>> {
        Transaction::query_rows(self, sql, values).await
    }
    async fn orm_execute(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.run(move |transaction| {
            Box::pin(async move { Ok(transaction.orm_execute(statement).await) })
        })
        .await
        .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?
    }
    async fn orm_query(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
        self.run(move |transaction| {
            Box::pin(async move { Ok(transaction.orm_query(statement).await) })
        })
        .await
        .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?
    }
    async fn orm_execute_unprepared(
        &self,
        sql: &str,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        let sql = sql.to_owned();
        self.run(move |transaction| {
            Box::pin(async move { Ok(transaction.orm_execute_unprepared(&sql).await) })
        })
        .await
        .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?
    }
    async fn commit(self: Box<Self>) -> Result<()> {
        Transaction::commit(*self).await
    }
    async fn rollback(self: Box<Self>) -> Result<()> {
        Transaction::rollback(*self).await
    }
}

#[cfg(test)]
mod transaction_runtime_tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    struct ReadDrop(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for ReadDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    struct TestTransaction {
        started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        dropped: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        disposed: Arc<AtomicUsize>,
    }
    impl Drop for TestTransaction {
        fn drop(&mut self) {
            self.disposed.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl PluginDataTransaction for TestTransaction {
        fn backend(&self) -> PluginDatabaseBackend {
            PluginDatabaseBackend::Postgres
        }
        async fn get_secret(&self, key: &str) -> Result<Option<String>> {
            if key == "pending" {
                let _guard = ReadDrop(self.dropped.lock().unwrap().take());
                self.started
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                std::future::pending::<Result<Option<String>>>().await
            } else {
                Ok(Some(
                    std::thread::current()
                        .name()
                        .unwrap_or("unnamed")
                        .to_owned(),
                ))
            }
        }
        async fn execute(&self, _sql: &str, _values: Vec<PluginSqlValue>) -> Result<u64> {
            Ok(1)
        }
        async fn query_json(
            &self,
            _sql: &str,
            _values: Vec<PluginSqlValue>,
        ) -> Result<Vec<serde_json::Value>> {
            Ok(vec![])
        }
        async fn commit(self: Box<Self>) -> Result<()> {
            Ok(())
        }
        async fn rollback(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }
    fn port(disposed: Arc<AtomicUsize>) -> TestTransaction {
        TestTransaction {
            started: Mutex::new(None),
            dropped: Mutex::new(None),
            disposed,
        }
    }

    #[test]
    fn transaction_read_uses_captured_host_runtime() {
        let host = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("transaction-host")
            .enable_all()
            .build()
            .unwrap();
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let disposed = Arc::new(AtomicUsize::new(0));
        let transaction = host.block_on(async {
            Transaction::new(
                Box::new(port(disposed.clone())),
                crate::runtime::RuntimeExecutor::current(),
            )
        });
        caller.block_on(async {
            assert_eq!(transaction.backend, PluginDatabaseBackend::Postgres);
            assert_eq!(
                transaction.get_secret("thread").await.unwrap().as_deref(),
                Some("transaction-host")
            );
            transaction.commit().await.unwrap();
        });
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_transaction_read_drops_host_operation_and_releases_mutex() {
        let disposed = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let transaction = Arc::new(Transaction::new(
            Box::new(TestTransaction {
                started: Mutex::new(Some(started_tx)),
                dropped: Mutex::new(Some(dropped_tx)),
                disposed: disposed.clone(),
            }),
            crate::runtime::RuntimeExecutor::current(),
        ));
        let pending = transaction.clone();
        let receipt = tokio::spawn(async move { pending.get_secret("pending").await });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        receipt.abort();
        let _ = receipt.await;
        tokio::time::timeout(Duration::from_secs(2), dropped_rx)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(2),
            transaction.execute("after-cancel", vec![]),
        )
        .await
        .unwrap()
        .unwrap();
        Arc::try_unwrap(transaction)
            .expect("cancelled host read must release its transaction")
            .rollback()
            .await
            .unwrap();
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn closed_transaction_rejects_further_reads() {
        let disposed = Arc::new(AtomicUsize::new(0));
        let transaction = Transaction::new(
            Box::new(port(disposed.clone())),
            crate::runtime::RuntimeExecutor::current(),
        );
        drop(transaction.inner.lock().await.take());
        let error = transaction.get_secret("never-read").await.unwrap_err();
        assert!(matches!(error, nuono_error::NuonoError::DatabaseQuery(_)));
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
    }
}
