//! 仅实现共享状态的内存宿主，复用 SDK 的真实状态入口。
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use nuono_plugin::db::{
    Database, PluginDataStore, PluginDatabaseBackend, PluginMigration, PluginMigrationForBackend,
    PluginSqlValue, PluginStateCollection, PluginStateScope, PluginStateValue,
};
use nuono_plugin::permission::{PermissionManager, PermissionSettings, PermissionStore};
use nuono_plugin::{NuonoError, NuonoResult, PluginContext, PluginHttpClient};
use serde_json::Value;

/// 为 `on_load` 和 `ctx.state(...)` 提供独立的内存测试环境。
///
/// 同一个宿主生成的上下文共享状态，不同宿主完全隔离；插件重建后仍可读取前次写入。
/// 只实现共享状态，不模拟 SQL、事务、迁移、密钥、权限策略或平台接入。
/// 不支持的存储操作明确报错，避免把未验证的行为当作成功。
/// `data_dir` 是自动回收的临时目录，测试中须让本对象比上下文活得更久。
///
/// 不会自动调用插件生命周期；测试自行调用 `plugin.on_load(&context).await`。
/// HTTP 句柄是 SDK 的真实客户端；本工具不拦截插件主动发起的外部请求。
#[derive(Debug)]
pub struct TestHost {
    store: MemoryState,
    services: nuono_plugin::PluginServiceRegistry,
    directory: tempfile::TempDir,
}

impl TestHost {
    /// 创建独立状态与临时数据目录，不连接数据库或启动服务。
    ///
    /// # Errors
    /// 临时目录无法创建时返回错误。
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            store: MemoryState::default(),
            services: nuono_plugin::PluginServiceRegistry::new(),
            directory: tempfile::tempdir()?,
        })
    }

    /// 返回当前测试宿主共享的插件服务注册表。
    #[must_use]
    pub fn services(&self) -> &nuono_plugin::PluginServiceRegistry {
        &self.services
    }

    /// 创建一个激活中的插件上下文，配置覆盖通过参数提供。
    ///
    /// 此方法不校验插件声明；`LiveConfig::bind` 会执行类型与字段校验。
    /// 如启动受管任务，测试结束前须调用 `context.tasks().shutdown().await`。
    ///
    /// # Errors
    /// 插件名不符合共享状态命名约定时返回错误。
    pub fn context(&self, plugin_name: &str, config: Value) -> NuonoResult<PluginContext> {
        PluginStateCollection::new(plugin_name, "test", PluginStateScope::global())?;
        let db = Database::new(self.store.clone());
        let http = PluginHttpClient::new();
        let data_dir = self.directory.path().join(plugin_name);
        Ok(PluginContext::from_parts(
            nuono_plugin::plugin::PluginContextParts {
                db,
                event_bus: Arc::new(nuono_plugin::core::event_bus::EventBus::new()),
                permission_manager: Arc::new(PermissionManager::new(UnavailablePermissions)),
                config: config.clone(),
                config_handle: nuono_plugin::PluginConfigHandle::new(config),
                config_updates: Default::default(),
                secrets: nuono_plugin::PluginSecretStore::new(
                    self.store.clone(),
                    plugin_name,
                    vec![],
                ),
                assets: nuono_plugin::PluginAssets::new(data_dir.join("assets"), http.clone()),
                data_dir,
                bot: None,
                http,
                embeddings: Default::default(),
                account_conversations: nuono_plugin::agent::account::AccountConversations::new(
                    UnavailableConversations,
                ),
                locales: Default::default(),
                cooldown_tracker: Arc::new(nuono_plugin::core::CooldownTracker::new()),
                command_stats: Arc::new(nuono_plugin::core::CommandStatsManager::new()),
                framework_control: nuono_plugin::core::framework_control::FrameworkControl::new(),
                tasks: Arc::new(nuono_plugin::PluginTaskScope::new(plugin_name)),
                media_links: Default::default(),
                services: self.services.clone(),
            },
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StateAddress {
    plugin: String,
    collection: String,
    scope: String,
    key: String,
}

impl StateAddress {
    fn new(collection: &PluginStateCollection, key: &str) -> Self {
        Self {
            plugin: collection.plugin_id().into(),
            collection: collection.name().into(),
            scope: collection.scope_key().into(),
            key: key.into(),
        }
    }

    fn belongs_to(&self, collection: &PluginStateCollection) -> bool {
        self.plugin == collection.plugin_id()
            && self.collection == collection.name()
            && self.scope == collection.scope_key()
    }
}

#[derive(Clone, Debug, Default)]
struct MemoryState(Arc<Mutex<BTreeMap<StateAddress, PluginStateValue>>>);

fn unsupported() -> NuonoError {
    NuonoError::DatabaseQuery("TestHost only supports ctx.state; use database integration tests for SQL, transactions, migrations, KV or secrets".into())
}

fn validate_key(key: &str) -> NuonoResult<()> {
    if key.is_empty() || key.len() > 200 || key.trim() != key || key.chars().any(char::is_control) {
        return Err(NuonoError::InvalidInput("invalid plugin state key".into()));
    }
    Ok(())
}

#[async_trait::async_trait]
impl PluginDataStore for MemoryState {
    // 并不真正模拟任何 SQL 数据库后端；返回此值仅为了满足底层 PluginDataStore 接口要求。
    fn backend(&self) -> PluginDatabaseBackend {
        PluginDatabaseBackend::Sqlite
    }

    async fn state_get(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> NuonoResult<Option<PluginStateValue>> {
        validate_key(key)?;
        Ok(self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&StateAddress::new(collection, key))
            .cloned())
    }

    async fn state_set(
        &self,
        collection: &PluginStateCollection,
        key: &str,
        value_version: i32,
        value_json: String,
    ) -> NuonoResult<()> {
        validate_key(key)?;
        if value_version <= 0 || value_json.len() > 64 * 1024 {
            return Err(NuonoError::InvalidInput(
                "state requires a positive version and at most 64 KiB of JSON".into(),
            ));
        }
        let value = serde_json::from_str(&value_json)?;
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(
            StateAddress::new(collection, key),
            PluginStateValue {
                value_version,
                value,
            },
        );
        Ok(())
    }

    async fn state_delete(
        &self,
        collection: &PluginStateCollection,
        key: &str,
    ) -> NuonoResult<bool> {
        validate_key(key)?;
        Ok(self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&StateAddress::new(collection, key))
            .is_some())
    }

    async fn state_list(
        &self,
        collection: &PluginStateCollection,
        after: Option<&str>,
        limit: u32,
    ) -> NuonoResult<Vec<(String, PluginStateValue)>> {
        if let Some(key) = after {
            validate_key(key)?;
        }
        if !(1..=100).contains(&limit) {
            return Err(NuonoError::InvalidInput(
                "state page limit must be 1..=100".into(),
            ));
        }
        Ok(self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(address, _)| {
                address.belongs_to(collection) && after.is_none_or(|key| address.key.as_str() > key)
            })
            .take(limit as usize)
            .map(|(address, value)| (address.key.clone(), value.clone()))
            .collect())
    }

    async fn get_secret(&self, _key: &str) -> NuonoResult<Option<String>> {
        Err(unsupported())
    }
    async fn set_secret(&self, _key: &str, _value: &str) -> NuonoResult<()> {
        Err(unsupported())
    }
    async fn delete_secret(&self, _key: &str) -> NuonoResult<()> {
        Err(unsupported())
    }
    async fn kv_get(&self, _namespace: &str, _key: &str) -> NuonoResult<Option<String>> {
        Err(unsupported())
    }
    async fn kv_set(&self, _namespace: &str, _key: &str, _value: &str) -> NuonoResult<()> {
        Err(unsupported())
    }
    async fn kv_delete(&self, _namespace: &str, _key: &str) -> NuonoResult<bool> {
        Err(unsupported())
    }
    async fn kv_list(&self, _namespace: &str) -> NuonoResult<Vec<(String, String)>> {
        Err(unsupported())
    }
    async fn kv_clear(&self, _namespace: &str) -> NuonoResult<u64> {
        Err(unsupported())
    }
    async fn migrate(&self, _namespace: &str, _migrations: &[PluginMigration]) -> NuonoResult<()> {
        Err(unsupported())
    }
    async fn migrate_backends(
        &self,
        _namespace: &str,
        _migrations: &[PluginMigrationForBackend],
    ) -> NuonoResult<()> {
        Err(unsupported())
    }
    async fn execute(&self, _sql: &str, _values: Vec<PluginSqlValue>) -> NuonoResult<u64> {
        Err(unsupported())
    }
    async fn query_json(
        &self,
        _sql: &str,
        _values: Vec<PluginSqlValue>,
    ) -> NuonoResult<Vec<Value>> {
        Err(unsupported())
    }
}

struct UnavailablePermissions;
#[async_trait::async_trait]
impl PermissionStore for UnavailablePermissions {
    async fn permission_settings(&self) -> Result<PermissionSettings, String> {
        Err("TestHost does not simulate host permission policy".into())
    }
}

struct UnavailableConversations;
#[async_trait::async_trait]
impl nuono_plugin::agent::account::AccountConversationPort for UnavailableConversations {
    async fn recent(
        &self,
        _account_id: i64,
    ) -> NuonoResult<Vec<nuono_plugin::agent::account::AccountMessage>> {
        Err(NuonoError::Validation(
            "TestHost does not provide AI conversations".into(),
        ))
    }

    async fn reply(
        &self,
        _account_id: i64,
        _text: &str,
        _system_prompt: &str,
    ) -> NuonoResult<String> {
        Err(NuonoError::Validation(
            "TestHost does not provide AI conversations".into(),
        ))
    }
}
