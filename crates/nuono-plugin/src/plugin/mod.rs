//! Native 插件的宿主侧 API 与生命周期契约。
//!
//! 插件通过 [`Plugin`] trait 暴露生命周期、事件、命令和工具，通过稳定接口连接宿主。
//! 插件对象由普通 Rust 工厂创建，生命周期由宿主管理。

mod runtime_handle;
pub use runtime_handle::PluginRuntime;

/// 本轮激活的来源；三种来源遵循相同的初始化和启用顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginActivationReason {
    Startup,
    Enable,
    Rebuild,
}
mod agent_skill;
pub use agent_skill::PluginAgentSkill;
mod web_page;
pub use web_page::{PluginWebAsset, PluginWebPage, PluginWebPagePlacement};
mod invocation;
#[doc(hidden)]
pub use invocation::with_invocation_guard;
mod services;
pub use nuono_app::plugin_role::PluginRole;
pub use services::{
    PluginServiceError, PluginServiceHandler, PluginServiceRegistry, PluginServiceResult,
};

use crate::agent::AgentTool;
use crate::api::Bot;
pub use crate::config::PluginConfigHandle;
use crate::core::command::Command;
use crate::core::config_schema::PluginConfigField;
use crate::core::event_bus::EventBus;
use crate::core::framework_control::FrameworkControl;
use crate::core::log_context;
use crate::core::permission::PermissionManager;
use crate::core::plugin_service::Plugins;
use crate::core::trigger::Trigger;
use crate::db::Database;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

pub mod db_models;
pub mod event;
mod message_listener;
mod state;
pub use message_listener::PluginMessageListener;
pub use state::{PluginStateScope, PluginStateStore, PluginStateValue};

pub use event::Event;

pub use db_models::PluginKVStore;

// ── 插件 secret 声明、规范化与作用域存储 ──
//
// secret（token、外部账号凭据等）是插件可声明、但只允许宿主命名空间化的托管值。插件用
// `secret_fields()` 声明插件内唯一的 key，宿主把它规范化为 `plugin.<plugin_name>.<key>`
// 存进 `runtime_secrets`；插件通过 `PluginContext::secrets` 用本地 key 存取，不接触全局键。
// 未声明字段的读写删必须报错，避免插件互读或读到其它插件的凭据。

/// 插件向宿主声明的一个 secret 字段。
///
/// `key` 是插件内唯一的字段名；宿主将其规范化为 `plugin.<plugin_name>.<key>` 存入
/// [`Database`] 的 `runtime_secrets`。`title`/`description` 供 Dashboard 展示，
/// `required` 表示该字段必须被设置。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginSecretField {
    /// 插件内唯一的字段 key（只允许 ASCII 字母数字、`-`、`_`）。
    pub key: String,
    /// 展示标题。
    pub title: String,
    /// 展示说明。
    pub description: String,
    /// 是否必填。
    #[serde(default)]
    pub required: bool,
}

impl PluginSecretField {
    pub fn new(
        key: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            title: title.into(),
            description: description.into(),
            required: false,
        }
    }

    /// 标记为必填。
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
}

/// 宿主侧产物：一个 secret 字段的全局唯一 canonical key 与其展示信息。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginSecretSchema {
    pub plugin_name: String,
    /// 规范键：`plugin.<plugin_name>.<field_key>`。
    pub key: String,
    pub title: String,
    pub description: String,
    pub required: bool,
}

/// 校验插件名 / 字段 key 是否可作为安全组件。
///
/// 只允许非空 ASCII 字母数字、`-`、`_`。这是一条硬约束：`.` 被排除，因此
/// `plugin.<name>.<key>` 可被无歧义解析，插件名或字段 key 也无法构造出跨插件键或目录穿越
/// 类键。空值一律拒绝——绝不允许静默改名（例如去掉前导点）造成碰撞。
pub fn validate_plugin_secret_component(component: &str) -> crate::error::Result<()> {
    if component.is_empty() {
        return Err(crate::error::NuonoError::Validation(
            "插件 secret 组件不能为空".into(),
        ));
    }
    if let Some(bad) = component
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return Err(crate::error::NuonoError::Validation(format!(
            "插件 secret 组件含非法字符 {bad:?}；只允许 ASCII 字母数字、`-`、`_`"
        )));
    }
    Ok(())
}

/// 生成插件 secret 的全局唯一 canonical key：`plugin.<plugin_name>.<field_key>`。
///
/// 插件名与字段 key 只允许安全字符，空值或非法字符直接报错，不会静默改名。
pub fn plugin_secret_canonical_key(
    plugin_name: &str,
    field_key: &str,
) -> crate::error::Result<String> {
    validate_plugin_secret_component(plugin_name)?;
    validate_plugin_secret_component(field_key)?;
    Ok(format!("plugin.{plugin_name}.{field_key}"))
}

/// 插件作用域的 secret 存取器。
///
/// `get` / `set` / `delete` 访问通过 [`Plugin::secret_fields`] 声明的字段；
/// `get_private` / `set_private` / `delete_private` 保存插件内部凭据，不公开到配置表单。
/// 内部把字段 key 规范化为 `plugin.<plugin_name>.<key>` 落到 [`Database`]，插件无需也不应
/// 传完整 canonical key。空值写入沿用删除语义（见 [`Database::set_secret`]）。
#[derive(Clone)]
pub struct PluginSecretStore {
    db: Database,
    plugin_name: String,
    allowed: std::collections::BTreeSet<String>,
}

impl std::fmt::Debug for PluginSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginSecretStore")
            .field("plugin_name", &self.plugin_name)
            .field("allowed_count", &self.allowed.len())
            .finish_non_exhaustive()
    }
}

impl PluginSecretStore {
    /// 构造插件作用域的 secret 存取器。
    ///
    /// 只能由宿主用**已验证**的 snapshot（经 [`Plugin::secret_fields`] 提取并校验后）构造，
    /// 构造入口供宿主适配器使用；普通插件通过 [`PluginContext::secrets`] 获得实例。
    /// 传入未验证字段是宿主内部的编程错误，debug 构建下直接 panic 以尽早暴露。
    pub fn new(
        db: impl nuono_app::plugin_data::PluginDataStore + 'static,
        plugin_name: impl Into<String>,
        fields: Vec<PluginSecretField>,
    ) -> Self {
        #[cfg(debug_assertions)]
        {
            let plugin_name = plugin_name.into();
            for field in &fields {
                plugin_secret_canonical_key(&plugin_name, &field.key)
                    .expect("PluginSecretStore 不能被未验证字段构造");
            }
            Self {
                db: Database::new(db),
                plugin_name,
                allowed: fields.into_iter().map(|f| f.key).collect(),
            }
        }
        #[cfg(not(debug_assertions))]
        {
            let allowed = fields.into_iter().map(|f| f.key).collect();
            Self {
                db: Database::new(db),
                plugin_name: plugin_name.into(),
                allowed,
            }
        }
    }

    fn private_key(&self, field_key: &str) -> crate::error::Result<String> {
        plugin_secret_canonical_key(&self.plugin_name, field_key)
    }

    /// 私有 secret 只允许插件内部使用，不出现在 secret schema 或 Dashboard 状态中。
    pub async fn get_private(&self, field_key: &str) -> crate::error::Result<Option<String>> {
        let canonical = self.private_key(field_key)?;
        self.db.get_secret(&canonical).await
    }

    /// 使用与业务事务相同的连接读取私有密钥。
    /// 读取能看到本事务尚未提交的写入，并保留插件键校验。
    /// 持有写事务时请用它，而不是再借另一条连接。
    pub async fn get_private_in_transaction(
        &self,
        transaction: &crate::db::Transaction,
        field_key: &str,
    ) -> crate::error::Result<Option<String>> {
        let canonical = self.private_key(field_key)?;
        transaction.get_secret(&canonical).await
    }

    pub async fn set_private(&self, field_key: &str, value: &str) -> crate::error::Result<()> {
        let canonical = self.private_key(field_key)?;
        self.db.set_secret(&canonical, value).await
    }

    /// 在 `ctx.db().begin()` 事务中与业务行原子写入私有密钥。
    /// 插件命名空间与键校验与 [`Self::set_private`] 相同。
    /// 丢弃或回滚事务也会回滚该密钥。
    pub async fn set_private_in_transaction(
        &self,
        transaction: &crate::db::Transaction,
        field_key: &str,
        value: &str,
    ) -> crate::error::Result<()> {
        let canonical = self.private_key(field_key)?;
        transaction.set_secret(&canonical, value).await
    }

    pub async fn delete_private(&self, field_key: &str) -> crate::error::Result<()> {
        let canonical = self.private_key(field_key)?;
        self.db.delete_secret(&canonical).await
    }

    /// 字段 key 是否被本插件声明。
    fn validate_key(&self, field_key: &str) -> crate::error::Result<String> {
        if !self.allowed.contains(field_key) {
            return Err(crate::error::NuonoError::PermissionDenied(format!(
                "插件未声明 secret 字段: {field_key}"
            )));
        }
        plugin_secret_canonical_key(&self.plugin_name, field_key)
    }

    /// 读取字段值；不存在或为空时返回 `None`。
    pub async fn get(&self, field_key: &str) -> crate::error::Result<Option<String>> {
        let canonical = self.validate_key(field_key)?;
        self.db.get_secret(&canonical).await
    }

    /// 写入字段值；空值等价于删除。
    pub async fn set(&self, field_key: &str, value: &str) -> crate::error::Result<()> {
        let canonical = self.validate_key(field_key)?;
        self.db.set_secret(&canonical, value).await
    }

    /// 删除字段值。
    pub async fn delete(&self, field_key: &str) -> crate::error::Result<()> {
        let canonical = self.validate_key(field_key)?;
        self.db.delete_secret(&canonical).await
    }

    /// 本插件声明的字段 key 列表。
    pub fn field_keys(&self) -> impl Iterator<Item = &str> {
        self.allowed.iter().map(String::as_str)
    }
}

/// 宿主构造 [`PluginContext`] 使用的内部部件包（供 `nuono-core` 与 `nuono-plugin-test` 装配）。
#[doc(hidden)]
pub struct PluginContextParts {
    pub db: Database,
    pub event_bus: Arc<EventBus>,
    pub permission_manager: Arc<PermissionManager>,
    pub config: Value,
    pub config_handle: crate::config::PluginConfigHandle,
    pub config_updates: crate::config::PluginConfigUpdates,
    pub secrets: PluginSecretStore,
    pub data_dir: PathBuf,
    pub bot: Option<Arc<Bot>>,
    pub http: PluginHttpClient,
    pub embeddings: crate::embedding::PluginEmbeddings,
    pub account_conversations: crate::agent::account::AccountConversations,
    pub locales: crate::LocaleService,
    pub assets: crate::PluginAssets,
    pub cooldown_tracker: Arc<crate::core::CooldownTracker>,
    pub command_stats: Arc<crate::core::CommandStatsManager>,
    pub framework_control: FrameworkControl,
    pub tasks: Arc<PluginTaskScope>,
    pub media_links: crate::media_link::PluginMediaLinks,
    pub services: PluginServiceRegistry,
}

impl std::fmt::Debug for PluginContextParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginContextParts")
            .field("data_dir", &self.data_dir)
            .field("has_bot", &self.bot.is_some())
            .finish_non_exhaustive()
    }
}

/// 宿主在激活阶段提供给插件的共享能力。
///
/// 所有字段保持私有并通过只读访问器暴露，以确保后续向 [`PluginContext`] 增补新宿主能力
/// 时严格遵守 SemVer 次版本兼容。插件不应自行关闭数据库或框架服务。
#[derive(Clone)]
pub struct PluginContext {
    pub(crate) db: Database,
    pub(crate) event_bus: Arc<EventBus>,
    pub(crate) permission_manager: Arc<PermissionManager>,
    /// 加载时的配置快照；热更新请使用 `config_handle().snapshot()`。
    pub(crate) config: Value,
    pub(crate) config_handle: crate::config::PluginConfigHandle,
    pub(crate) config_updates: crate::config::PluginConfigUpdates,
    pub(crate) secrets: PluginSecretStore,
    pub(crate) data_dir: PathBuf,
    pub(crate) bot: Option<Arc<Bot>>,
    pub(crate) http: PluginHttpClient,
    pub(crate) embeddings: crate::embedding::PluginEmbeddings,
    pub(crate) account_conversations: crate::agent::account::AccountConversations,
    pub(crate) locales: crate::LocaleService,
    pub(crate) assets: crate::PluginAssets,
    pub(crate) cooldown_tracker: Arc<crate::core::CooldownTracker>,
    pub(crate) command_stats: Arc<crate::core::CommandStatsManager>,
    pub(crate) framework_control: FrameworkControl,
    pub(crate) tasks: Arc<PluginTaskScope>,
    pub(crate) media_links: crate::media_link::PluginMediaLinks,
    pub(crate) services: PluginServiceRegistry,
}

impl std::fmt::Debug for PluginContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginContext")
            .field("data_dir", &self.data_dir)
            .field("has_bot", &self.bot.is_some())
            .finish_non_exhaustive()
    }
}

impl PluginContext {
    /// 宿主内部装配入口。
    #[doc(hidden)]
    pub fn from_parts(parts: PluginContextParts) -> Self {
        Self {
            db: parts.db,
            event_bus: parts.event_bus,
            permission_manager: parts.permission_manager,
            config: parts.config,
            config_handle: parts.config_handle,
            config_updates: parts.config_updates,
            secrets: parts.secrets,
            data_dir: parts.data_dir,
            bot: parts.bot,
            http: parts.http,
            embeddings: parts.embeddings,
            account_conversations: parts.account_conversations,
            locales: parts.locales,
            assets: parts.assets,
            cooldown_tracker: parts.cooldown_tracker,
            command_stats: parts.command_stats,
            framework_control: parts.framework_control,
            tasks: parts.tasks,
            media_links: parts.media_links,
            services: parts.services,
        }
    }

    /// 加载时的配置快照（热更新请使用 [`Self::current_config`] 或 [`Self::config_handle`]）。
    pub fn config(&self) -> &Value {
        &self.config
    }

    /// 数据库持久化句柄（自动绑定当前插件命名空间与事务支持）。
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// 进程内事件总线句柄。
    pub fn event_bus(&self) -> &Arc<EventBus> {
        &self.event_bus
    }

    /// 权限管理器句柄。
    pub fn permission_manager(&self) -> &Arc<PermissionManager> {
        &self.permission_manager
    }

    /// 运行时热配置句柄。
    pub fn config_handle(&self) -> &crate::config::PluginConfigHandle {
        &self.config_handle
    }

    /// 持久配置补丁提交入口。
    pub fn config_updates(&self) -> &crate::config::PluginConfigUpdates {
        &self.config_updates
    }

    /// 当前插件作用域的密钥存储。
    pub fn secrets(&self) -> &PluginSecretStore {
        &self.secrets
    }

    /// 当前插件数据目录路径。
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// 进程级 routed Bot 动作句柄。
    pub fn bot(&self) -> Option<&Arc<Bot>> {
        self.bot.as_ref()
    }

    /// 默认预配置的 HTTP 客户端。
    pub fn http(&self) -> &PluginHttpClient {
        &self.http
    }

    /// 宿主托管的 embedding 模型入口。
    pub fn embeddings(&self) -> &crate::embedding::PluginEmbeddings {
        &self.embeddings
    }

    /// 账号级对话代理入口。
    pub fn account_conversations(&self) -> &crate::agent::account::AccountConversations {
        &self.account_conversations
    }

    /// 语言与本地化服务入口。
    pub fn locales(&self) -> &crate::LocaleService {
        &self.locales
    }

    /// 插件静态素材与缓存管理入口。
    pub fn assets(&self) -> &crate::PluginAssets {
        &self.assets
    }

    /// 指令冷却追踪器。
    pub fn cooldown_tracker(&self) -> &Arc<crate::core::CooldownTracker> {
        &self.cooldown_tracker
    }

    /// 指令调用统计管理器。
    pub fn command_stats(&self) -> &Arc<crate::core::CommandStatsManager> {
        &self.command_stats
    }

    /// 框架控制信号发送器。
    pub fn framework_control(&self) -> &FrameworkControl {
        &self.framework_control
    }

    /// 当前插件的受管后台任务作用域。
    pub fn tasks(&self) -> &Arc<PluginTaskScope> {
        &self.tasks
    }

    /// 宿主签发的短时效媒体链接入口。
    pub fn media_links(&self) -> &crate::media_link::PluginMediaLinks {
        &self.media_links
    }

    /// 向宿主服务总线注册当前插件的跨插件动作处理器；插件停用或卸载时由宿主自动摘除。
    pub fn register_service(&self, handler: Arc<dyn PluginServiceHandler>) {
        if self.tasks.is_closed() {
            tracing::warn!(
                plugin = %self.tasks.plugin_name(),
                "插件作用域已关闭，忽略延迟注册的服务处理器"
            );
            return;
        }
        self.services.register(self.tasks.plugin_name(), handler);
    }

    /// 返回宿主共享服务注册表引用，供插件内部子组件或后台任务按需动态调用其他插件服务。
    #[must_use]
    pub fn services(&self) -> &PluginServiceRegistry {
        &self.services
    }

    /// 检查指定服务插件当前是否在线并已注册处理器。
    #[must_use]
    pub fn has_service(&self, plugin_id: &str) -> bool {
        self.services.has_service(plugin_id)
    }

    /// 通过宿主通用服务总线实时调用指定插件的命名动作。
    pub async fn call_service(
        &self,
        plugin_id: &str,
        action: &str,
        payload: serde_json::Value,
    ) -> PluginServiceResult<serde_json::Value> {
        self.services.call(plugin_id, action, payload).await
    }

    /// 通过宿主通用服务总线实时发起强类型序列化/反序列化动作调用。
    pub async fn call_service_typed<Req, Resp>(
        &self,
        plugin_id: &str,
        action: &str,
        request: &Req,
    ) -> PluginServiceResult<Resp>
    where
        Req: serde::Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        self.services.call_typed(plugin_id, action, request).await
    }

    /// 解析 `storage_declaration` 中声明的外部资源（External asset）在数据库迁移或目录重定位后的实际路径。
    /// 请通过此稳定 key 查询路径，不要缓存首次部署时的原始绝对路径。
    pub fn external_asset_path(
        &self,
        key: &str,
        declared: &std::path::Path,
    ) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            declared.is_absolute(),
            "external asset default must be absolute"
        );
        anyhow::ensure!(
            !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
            "invalid external asset key"
        );
        let data = self
            .data_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("plugin data root has no parent"))?;
        let receipt = data.join(".portable-external/completed.json");
        if !receipt.exists() {
            return Ok(declared.into());
        }
        anyhow::ensure!(
            std::fs::metadata(&receipt)?.len() <= 4 * 1024 * 1024,
            "external asset receipt too large"
        );
        let value = nuono_domain::scalar::parse_json(&std::fs::read_to_string(receipt)?)?;
        let identity = format!("{}/{}", self.tasks.plugin, key);
        let mapped = value
            .get("assets")
            .and_then(Value::as_array)
            .is_some_and(|assets| {
                assets.iter().any(|asset| {
                    asset.get("identity").and_then(Value::as_str) == Some(identity.as_str())
                        && asset.get("restore").and_then(Value::as_bool) == Some(true)
                        && asset.get("declared_path").and_then(Value::as_str) == declared.to_str()
                })
            });
        if !mapped {
            return Ok(declared.into());
        }
        let target = value
            .get("targets")
            .and_then(|v| v.get(&identity))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("external asset receipt has no mapped path"))?;
        let path = PathBuf::from(target);
        anyhow::ensure!(
            path.is_absolute(),
            "external asset receipt contains a relative path"
        );
        Ok(path)
    }

    /// 当前激活来自启动、手动启用还是实例重建；行为顺序在三种情况下保持一致。
    pub fn activation_reason(&self) -> PluginActivationReason {
        self.tasks.activation_reason
    }

    /// Obscura 页面抓取入口，绑定本插件生命周期；需启用 SDK 的 `obscura` feature。
    pub fn browser(&self) -> crate::browser::PluginBrowser {
        crate::browser::PluginBrowser::for_scope(self.tasks.clone())
    }

    /// 在线程里生成图片或执行其它 CPU 工作，配额与生命周期由宿主管理。
    pub async fn render<T, F>(&self, name: &str, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        self.tasks.render(name, job).await
    }

    /// 读取一次当前宿主策略，再把该快照传给编码器
    /// （在渲染闭包内）。此处绝不重编码已有上传。
    pub async fn image_output_settings(&self) -> Result<crate::ImageOutputSettings> {
        self.tasks.image_output_settings().await
    }
    /// 启动一个随插件生命周期管理的后台任务。
    ///
    /// 任务 panic 由宿主捕获并记日志；`on_disable` / `on_unload` 之前宿主会 abort
    /// 并等待所有受管任务退出，插件不再需要自己维护 JoinHandle 列表。
    pub fn spawn<F>(&self, name: &str, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.tasks.spawn(name, fut)
    }

    pub async fn get_config(&self, _plugin_name: &str, key: &str) -> Option<Value> {
        self.config_handle.snapshot().get(key).cloned()
    }

    /// 按声明的类型读取当前配置，每次调用取得同一份完整快照。
    pub fn typed_config<T: crate::ConfigSchema>(&self) -> Result<T> {
        T::from_value(self.current_config().as_ref())
    }

    pub fn current_config(&self) -> Arc<Value> {
        self.config_handle.snapshot()
    }

    /// 本插件的 KV 存储；命名空间由宿主身份确定。
    /// 插件注册名，用于日志和业务命名空间。
    pub fn plugin_name(&self) -> &str {
        self.tasks.plugin_name()
    }

    /// 当前插件的持久订阅。订阅保存在宿主数据库，禁用插件不会删除用户订阅。
    pub fn subscriptions(&self) -> crate::subscriptions::Subscriptions {
        crate::subscriptions::Subscriptions::new(self.db.clone(), self.plugin_name())
    }

    /// 定时任务在插件停用或重建时自动取消。
    pub fn schedule<F, Fut>(
        &self,
        name: &str,
        schedule: crate::Schedule,
        job: F,
    ) -> Result<crate::ScheduledTask>
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        self.tasks.schedule(name, schedule, job)
    }

    pub fn kv(&self) -> db_models::PluginKVStore {
        db_models::PluginKVStore::from_database(self.db.clone(), self.secrets.plugin_name.clone())
    }

    pub fn kv_store(&self, plugin_name: &str) -> db_models::PluginKVStore {
        db_models::PluginKVStore::from_database(self.db.clone(), plugin_name)
    }

    /// 打开当前插件的共享状态集合。宿主绑定插件身份。
    ///
    /// # Errors
    /// 插件身份或集合名不符合持久化命名规则时返回错误。
    pub fn state(
        &self,
        collection: &str,
        scope: PluginStateScope,
    ) -> crate::error::Result<PluginStateStore> {
        let collection =
            crate::db::PluginStateCollection::new(self.plugin_name(), collection, scope)?;
        Ok(PluginStateStore::new(self.db.clone(), collection))
    }

    /// 确保插件数据目录存在（异步创建）
    pub async fn ensure_data_dir(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.data_dir)
            .await
            .map_err(|e| anyhow::anyhow!("创建数据目录失败: {}", e))
    }
}

/// 插件 HTTP 客户端
///
/// 封装 `reqwest::Client`，提供 GET/POST/JSON 便捷方法。
/// 默认超时 10 秒，自动 follow redirect。需要自定义超时或代理时用
/// [`PluginHttpClient::builder`]，例如下载大文件的插件可放宽总超时：
///
/// 文档测试会实际构造客户端；此步骤不会连接代理或发送请求。
///
/// ```rust
/// use nuono_plugin::PluginHttpClient;
/// use std::time::Duration;
///
/// let http = PluginHttpClient::builder()
///     .timeout(Duration::from_secs(120))
///     .proxy(Some("http://127.0.0.1:7890"))
///     .try_build()?;
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Clone)]
pub struct PluginHttpClient {
    client: reqwest::Client,
    settings: Arc<PluginHttpSettings>,
}

impl std::fmt::Debug for PluginHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHttpClient")
            .field("timeout", &self.settings.timeout)
            .field("connect_timeout", &self.settings.connect_timeout)
            .field("has_proxy", &self.settings.proxy.is_some())
            .field("follow_redirects", &self.settings.follow_redirects)
            .field("retry_protocol_nacks", &self.settings.retry_protocol_nacks)
            .finish_non_exhaustive()
    }
}

// 保留生效的 SDK 设置，包括旧版 builder 的回退。
// 不要 derive Debug：显式代理 URL 可能含凭据。
#[derive(Clone)]
struct PluginHttpSettings {
    timeout: Option<std::time::Duration>,
    connect_timeout: Option<std::time::Duration>,
    proxy: Option<String>,
    follow_redirects: bool,
    retry_protocol_nacks: bool,
}
impl PluginHttpSettings {
    fn defaults() -> Self {
        Self {
            timeout: Some(std::time::Duration::from_secs(10)),
            connect_timeout: Some(std::time::Duration::from_secs(5)),
            proxy: None,
            follow_redirects: true,
            retry_protocol_nacks: true,
        }
    }
    fn reqwest_defaults() -> Self {
        Self {
            timeout: None,
            connect_timeout: None,
            ..Self::defaults()
        }
    }
}

/// [`PluginHttpClient`] 的构造器，字段与 `ctx.http` 默认值一致。
#[derive(Clone)]
pub struct PluginHttpClientBuilder {
    settings: PluginHttpSettings,
}

impl std::fmt::Debug for PluginHttpClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHttpClientBuilder")
            .field("timeout", &self.settings.timeout)
            .field("connect_timeout", &self.settings.connect_timeout)
            .field("has_proxy", &self.settings.proxy.is_some())
            .field("follow_redirects", &self.settings.follow_redirects)
            .field("retry_protocol_nacks", &self.settings.retry_protocol_nacks)
            .finish()
    }
}

impl PluginHttpClientBuilder {
    /// 请求总超时（含响应体读取），默认 10 秒。
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.settings.timeout = Some(timeout);
        self
    }

    /// 建立连接的超时，默认 5 秒。
    pub fn connect_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.settings.connect_timeout = Some(timeout);
        self
    }

    /// HTTP(S) 代理地址，如 `http://127.0.0.1:7890`；`None` 沿用 reqwest 默认代理策略。
    pub fn proxy(mut self, proxy: Option<&str>) -> Self {
        self.settings.proxy = proxy.map(str::to_string);
        self
    }

    /// 是否自动跟随重定向，默认沿用 reqwest 策略。关闭后将 3xx 原响应交给调用者。
    /// 对需要明确处理构造失败的客户端，使用 [`Self::try_build`]。
    pub fn follow_redirects(mut self, enabled: bool) -> Self {
        self.settings.follow_redirects = enabled;
        self
    }

    /// 是否沿用 reqwest 默认的 protocol NACK 重试。关闭后所有请求都不自动重试。
    /// 这不限制调用方主动发送新请求，也不能判断远端是否已处理一个断开的请求。
    pub fn retry_protocol_nacks(mut self, enabled: bool) -> Self {
        self.settings.retry_protocol_nacks = enabled;
        self
    }

    fn configured_client(&self, strict_proxy: bool) -> Result<PluginHttpClient> {
        let mut settings = self.settings.clone();
        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = settings.timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(timeout) = settings.connect_timeout {
            builder = builder.connect_timeout(timeout);
        }
        if !settings.follow_redirects {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        if !settings.retry_protocol_nacks {
            builder = builder.retry(reqwest::retry::never());
        }
        if let Some(proxy) = settings.proxy.as_deref() {
            match reqwest::Proxy::all(proxy) {
                Ok(proxy) => builder = builder.proxy(proxy),
                Err(_) if strict_proxy => return Err(anyhow::anyhow!("插件 HTTP 代理地址无效")),
                Err(_) => {
                    // 保留旧版构建行为，且不记录代理凭据。
                    tracing::warn!("插件 HTTP 代理地址无效，忽略");
                    settings.proxy = None;
                }
            }
        }
        let client = builder
            .build()
            .map_err(|_| anyhow::anyhow!("插件 HTTP 客户端构造失败"))?;
        Ok(PluginHttpClient {
            client,
            settings: Arc::new(settings),
        })
    }

    /// 严格构造：无效代理或客户端构造失败返回错误，不回落到默认 HTTP 策略。
    pub fn try_build(self) -> Result<PluginHttpClient> {
        self.configured_client(true)
    }

    /// 保留既有默认构造行为。默认策略构造失败时使用 reqwest 默认客户端。
    ///
    /// 禁重定向/重试的策略绝不静默回落；该模式构造失败会 panic。需要处理
    /// 构造失败的业务必须使用 [`Self::try_build`] 或
    /// [`PluginHttpClient::without_redirects_and_retries`]。
    pub fn build(self) -> PluginHttpClient {
        match self.configured_client(false) {
            Ok(client) => client,
            Err(_) if self.settings.follow_redirects && self.settings.retry_protocol_nacks => {
                PluginHttpClient {
                    client: reqwest::Client::default(),
                    settings: Arc::new(PluginHttpSettings::reqwest_defaults()),
                }
            }
            Err(_) => panic!("插件 HTTP 限定策略构造失败；请使用 try_build 处理错误"),
        }
    }
}

impl PluginHttpClient {
    pub fn new() -> Self {
        Self::builder().build()
    }

    pub fn builder() -> PluginHttpClientBuilder {
        PluginHttpClientBuilder {
            settings: PluginHttpSettings::defaults(),
        }
    }

    /// 在当前实例的有效超时、连接超时、代理和策略基础上构造衍生客户端。
    /// 衍生客户端有独立连接池；插件自己的任务 scope 和并发许可不由该方法改动。
    pub fn rebuild_builder(&self) -> PluginHttpClientBuilder {
        PluginHttpClientBuilder {
            settings: self.settings.as_ref().clone(),
        }
    }

    /// 用当前配置派生不跟随重定向、也不重试协议错误的客户端。
    /// 构造失败返回错误，绝不使用策略不同的默认客户端。
    /// 调用者仍需在同一受管任务与并发许可内发起请求，并自行处理 3xx/未知结果。
    pub fn without_redirects_and_retries(&self) -> Result<Self> {
        self.rebuild_builder()
            .follow_redirects(false)
            .retry_protocol_nacks(false)
            .try_build()
    }

    pub async fn get_text(&self, url: &str) -> Result<String> {
        let resp = self.client.get(url).send().await?;
        Ok(resp.text().await?)
    }

    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.client.get(url).send().await?;
        Ok(resp.json::<T>().await?)
    }

    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.client.get(url).send().await?;
        Ok(resp.bytes().await?.to_vec())
    }

    pub async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: &impl serde::Serialize,
    ) -> Result<T> {
        let resp = self.client.post(url).json(body).send().await?;
        Ok(resp.json::<T>().await?)
    }

    pub async fn post_text(&self, url: &str, body: &str) -> Result<String> {
        let resp = self.client.post(url).body(body.to_string()).send().await?;
        Ok(resp.text().await?)
    }

    pub fn raw(&self) -> &reqwest::Client {
        &self.client
    }
}

impl Default for PluginHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginResponse {
    /// 回复纯文本消息 — 自动识别群聊/私聊
    ///
    /// 从 OneBot 事件 JSON 中提取 `group_id` / `user_id` / `message_type`，
    /// 构造正确的 OneBot action JSON。
    pub fn text(event: &Value, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        // 原样保留平台原始 ID（无论是数字还是非空字符串），由各平台编解码器按需转换数值。
        let valid_id = |value: &Value| {
            value.is_number() || value.as_str().is_some_and(|v| !v.trim().is_empty())
        };
        let uid = event
            .get("user_id")
            .filter(|v| valid_id(v))
            .cloned()
            .unwrap_or(Value::Null);
        let gid = event.get("group_id").filter(|v| valid_id(v));
        let channel = event.get("channel_id").filter(|v| valid_id(v));
        let mut params = serde_json::json!({"message":[{"type":"text","data":{"text":msg}}]});
        if let Some(channel) =
            channel.filter(|_| event.get("message_type").and_then(Value::as_str) == Some("channel"))
        {
            params["message_type"] = serde_json::json!("channel");
            params["channel_id"] = channel.clone();
            if let Some(guild) = event.get("guild_id").or(gid) {
                params["guild_id"] = guild.clone();
            }
        } else if let Some(group) = gid {
            params["message_type"] = serde_json::json!("group");
            params["group_id"] = group.clone();
        } else {
            params["message_type"] = serde_json::json!("private");
            params["user_id"] = uid;
        }
        if let Some(channel) = channel {
            params["channel_id"] = channel.clone();
        }

        Self::Reply(serde_json::json!({
            "action": "send_msg",
            "params": params,
        }))
    }

    /// 只回复群消息（传入 group_id 和文本）
    pub fn group_text(group_id: i64, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Self::Reply(serde_json::json!({
            "action": "send_msg",
            "params": {
                "message_type": "group",
                "group_id": group_id,
                "message": [{ "type": "text", "data": { "text": msg } }]
            }
        }))
    }

    /// 只回复私聊消息（传入 user_id 和文本）
    pub fn private_text(user_id: i64, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Self::Reply(serde_json::json!({
            "action": "send_msg",
            "params": {
                "message_type": "private",
                "user_id": user_id,
                "message": [{ "type": "text", "data": { "text": msg } }]
            }
        }))
    }

    /// 回复 @某人 + 文本（仅群聊有效，私聊自动降级为普通文本）
    pub fn at_text(event: &Value, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        let gid = event["group_id"].as_i64();
        let uid = event["user_id"].as_i64().unwrap_or(0);
        let channel_id = event.get("channel_id").cloned();

        match gid {
            Some(g) => {
                let mut params = serde_json::json!({
                    "message_type": "group",
                    "group_id": g,
                    "message": [
                        { "type": "at", "data": { "qq": uid.to_string() } },
                        { "type": "text", "data": { "text": format!(" {}", msg) } }
                    ]
                });
                if let Some(channel_id) = channel_id {
                    params["channel_id"] = channel_id;
                }
                Self::Reply(serde_json::json!({
                    "action": "send_msg",
                    "params": params,
                }))
            }
            None => Self::text(event, msg),
        }
    }

    /// 发送图片（传入图片 URL 或 base64://... ）
    pub fn image(event: &Value, file_url: impl Into<String>) -> Self {
        let file_url = file_url.into();
        let gid = event["group_id"].as_i64();
        let uid = event["user_id"].as_i64().unwrap_or(0);
        let channel_id = event.get("channel_id").cloned();

        let mut params = match gid {
            Some(g) => serde_json::json!({
                    "message_type": "group",
                    "group_id": g,
                    "message": [{ "type": "image", "data": { "file": file_url } }]
            }),
            None => serde_json::json!({
                    "message_type": "private",
                    "user_id": uid,
                    "message": [{ "type": "image", "data": { "file": file_url } }]
            }),
        };
        if let Some(channel_id) = channel_id {
            params["channel_id"] = channel_id;
        }

        Self::Reply(serde_json::json!({
            "action": "send_msg",
            "params": params,
        }))
    }

    /// 发送原始 OneBot action JSON（用于特殊 API 调用如 set_group_ban 等）
    pub fn action(action: &str, params: Value) -> Self {
        Self::Reply(serde_json::json!({
            "action": action,
            "params": params,
        }))
    }
}

/// 插件对单次事件处理的控制流决定。
///
/// `Continue` 允许后续插件或触发器继续处理，`Stop` 仅阻断后续链路，`Reply` 在回复后
/// 阻断。`WaitReply` 还会登记一个按 `session_id` 匹配的一次性等待；同一会话的新登记
/// 可能覆盖旧等待，超时后的清理由等待管理器负责。
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
#[non_exhaustive]
pub enum PluginResponse {
    Continue,
    Stop,
    Reply(Value),
    WaitReply {
        reply_message: Value,
        session_id: String,
        timeout_secs: u32,
        callback_id: String,
        plugin_name: String,
        event_context: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum PluginWebAccessPolicy {
    /// 公开回调接口；插件必须自行校验请求签名或敏感载荷。
    Public,
    /// 要求已登录的 Web 控制台会话。
    Authenticated,
    /// 要求具备 `Capability::ManageRuntime` 权限的管理员会话。
    ManageRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginWebRoute {
    pub method: http::Method,
    pub path: String,
    pub policy: PluginWebAccessPolicy,
    /// 请求体最大字节数，由宿主在读取缓冲请求体前强制拦截。
    /// 默认为 2 MiB；允许声明的范围为 1 字节至 32 MiB。
    pub max_body_bytes: usize,
}

impl PluginWebRoute {
    pub fn new(
        method: http::Method,
        path: impl Into<String>,
        policy: PluginWebAccessPolicy,
    ) -> Self {
        Self {
            method,
            path: path.into(),
            policy,
            max_body_bytes: 2 * 1024 * 1024,
        }
    }

    pub fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginWebUser {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub user_id: i64,
    pub username: String,
    pub role: String,
}

impl PluginWebUser {
    /// 将持久化角色字符串解析为强类型 [`crate::auth::Role`]。
    pub fn role_enum(&self) -> crate::auth::Role {
        crate::auth::Role::from_db_value(&self.role)
    }

    /// 当前用户角色是否具备指定框架内置能力或插件扩展能力。
    ///
    /// 未知或非法的角色字符串一律返回 `false`，避免隐式降级绕过权限检查。
    pub fn has_capability(&self, capability: &str) -> bool {
        let Some(role) = crate::auth::Role::parse(&self.role) else {
            return false;
        };
        role.capabilities()
            .iter()
            .any(|cap| cap.as_str() == capability)
            || crate::auth::has_extension_capability(role, capability)
    }

    /// 要求当前用户具备指定框架内置能力或插件扩展能力，否则返回 [`crate::NuonoError::PermissionDenied`]。
    pub fn require_capability(&self, capability: &str) -> crate::NuonoResult<()> {
        if self.has_capability(capability) {
            Ok(())
        } else {
            Err(crate::NuonoError::PermissionDenied(format!(
                "缺少所需权限: {capability}"
            )))
        }
    }
}

pub struct PluginWebRequest {
    pub method: http::Method,
    pub path: String,
    pub query: String,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
    pub user: Option<PluginWebUser>,
}

impl std::fmt::Debug for PluginWebRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginWebRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("has_query", &!self.query.is_empty())
            .field("header_count", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .field("has_user", &self.user.is_some())
            .finish()
    }
}

impl PluginWebRequest {
    /// 构造一个基础插件 Web 请求（便于测试与构造器链式调用）。
    pub fn new(method: http::Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: String::new(),
            headers: http::HeaderMap::new(),
            body: Vec::new(),
            user: None,
        }
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    pub fn with_headers(mut self, headers: http::HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn with_json_body(mut self, value: &impl serde::Serialize) -> crate::NuonoResult<Self> {
        self.body = serde_json::to_vec(value)?;
        Ok(self)
    }

    pub fn with_user(mut self, user: Option<PluginWebUser>) -> Self {
        self.user = user;
        self
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
#[non_exhaustive]
pub enum PluginWebResponse {
    Html(String),
    Json(Value),
    Raw {
        content_type: String,
        body: Vec<u8>,
    },
    /// 自定义完整 HTTP 响应。宿主会校验状态码与响应头格式，并拒绝传输层/分帧相关的响应头（如 `Content-Length`）。
    Http {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Status(u16, String),
    Redirect(String),
    NotFound,
}

impl std::fmt::Debug for PluginWebResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Html(html) => f.debug_struct("Html").field("bytes", &html.len()).finish(),
            Self::Json(_) => f.debug_struct("Json").finish_non_exhaustive(),
            Self::Raw { content_type, body } => f
                .debug_struct("Raw")
                .field("content_type", content_type)
                .field("bytes", &body.len())
                .finish(),
            Self::Http {
                status,
                headers,
                body,
            } => f
                .debug_struct("Http")
                .field("status", status)
                .field("header_count", &headers.len())
                .field("bytes", &body.len())
                .finish(),
            Self::Status(code, _) => f
                .debug_struct("Status")
                .field("code", code)
                .finish_non_exhaustive(),
            Self::Redirect(_) => f.debug_struct("Redirect").finish_non_exhaustive(),
            Self::NotFound => f.write_str("NotFound"),
        }
    }
}

impl PluginWebResponse {
    pub fn not_found() -> Self {
        PluginWebResponse::NotFound
    }
}

/// 插件声明自己关心的入站事件类别。
///
/// 宿主在派发前按此过滤：未声明类别的事件根本不会进入 `on_event`，插件无需再写
/// `if event.post_type() != "message"` 这类前置判断。默认 [`PluginEventFilter::ALL`]
/// 保持既有行为。类别可用 `|` 组合：
///
/// ```rust
/// use nuono_plugin::{Plugin, PluginEventFilter};
/// # struct MyPlugin;
///
/// impl Plugin for MyPlugin {
/// #     fn name(&self) -> &str { "MyPlugin" }
/// #     fn version(&self) -> &str { "1.0.0" }
/// #     fn description(&self) -> &str { "事件过滤示例" }
///     fn event_filter(&self) -> PluginEventFilter {
///         PluginEventFilter::MESSAGES | PluginEventFilter::NOTICES
///     }
/// }
/// # let filter = MyPlugin.event_filter();
/// # assert!(filter.accepts("message"));
/// # assert!(filter.accepts("notice"));
/// # assert!(!filter.accepts("request"));
/// # assert!(!filter.accepts("meta_event"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PluginEventFilter(u8);

impl PluginEventFilter {
    /// 不接收任何事件（纯命令/工具型插件可用）。
    pub const NONE: Self = Self(0);
    /// 消息事件（`post_type == "message"`）。
    pub const MESSAGES: Self = Self(0b0001);
    /// 通知事件（戳一戳、群成员变动等）。
    pub const NOTICES: Self = Self(0b0010);
    /// 请求事件（好友/加群申请）。
    pub const REQUESTS: Self = Self(0b0100);
    /// 元事件（lifecycle、heartbeat）。
    pub const META_EVENTS: Self = Self(0b1000);
    /// 全部事件，即 [`Plugin::event_filter`] 的默认值。
    pub const ALL: Self = Self(0b1111);

    /// 该类别的事件是否会被派发给插件。未知 `post_type` 只被 [`Self::ALL`] 接收。
    pub fn accepts(&self, post_type: &str) -> bool {
        let bit = match post_type {
            "message" => Self::MESSAGES.0,
            "notice" => Self::NOTICES.0,
            "request" => Self::REQUESTS.0,
            "meta_event" => Self::META_EVENTS.0,
            _ => return self.0 == Self::ALL.0,
        };
        self.0 & bit != 0
    }
}

impl std::ops::BitOr for PluginEventFilter {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for PluginEventFilter {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// 随插件生命周期管理的后台任务作用域。
///
/// 插件用 [`PluginContext::spawn`] 启动轮询、订阅等长驻任务，宿主负责在插件禁用、
/// 卸载或实例重建前统一 abort 并等待退出（quiescence），取代插件各自手写
/// "JoinHandle 列表 + drain + abort + await" 的样板。panic 会被捕获并记入宿主日志，
/// 不会带崩进程。普通异步后台任务每个作用域最多 256 个；绘图和浏览器任务使用各自配额。
pub struct PluginTaskScope {
    plugin: String,
    business_gate: nuono_app::business_runtime::BusinessGate,
    activation_reason: PluginActivationReason,
    closed: std::sync::atomic::AtomicBool,
    executor: Option<crate::runtime::RuntimeExecutor>,
    tasks: std::sync::Mutex<Vec<Box<dyn crate::runtime::RuntimeTask>>>,
    async_slots: Arc<tokio::sync::Semaphore>,
    activity: Arc<TaskActivity>,
    cleanup: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    render_pool: Arc<crate::RenderPool>,
    image_output: crate::PluginImageOutput,
    closed_notify: tokio::sync::Notify,
}

impl std::fmt::Debug for PluginTaskScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginTaskScope")
            .field("plugin", &self.plugin)
            .field("activation_reason", &self.activation_reason)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

const MAX_PLUGIN_ASYNC_TASKS: usize = 256;

#[cfg(feature = "mcp")]
mod mcp_scope;

#[derive(Default)]
struct TaskActivity {
    active: std::sync::atomic::AtomicUsize,
    changed: tokio::sync::Notify,
}

impl TaskActivity {
    fn register(self: &Arc<Self>) -> TaskCompletion {
        self.active
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        TaskCompletion(self.clone())
    }

    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}

struct TaskCompletion(Arc<TaskActivity>);

impl Drop for TaskCompletion {
    fn drop(&mut self) {
        if self
            .0
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1
        {
            self.0.changed.notify_waiters();
        }
    }
}

// 字段声明顺序不可调换：确保先析构 `future` 捕获的资源，再归还并发许可（`_slot`），
// 最后才触发完成通知（`_completion`），即使该 future 从未被轮询也能在 shutdown 前被正确观测到。
struct ScopedFuture<F> {
    future: std::pin::Pin<Box<F>>,
    _slot: tokio::sync::OwnedSemaphorePermit,
    _completion: TaskCompletion,
}

impl<F: std::future::Future<Output = ()>> std::future::Future for ScopedFuture<F> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

struct ScopedBlocking<F> {
    job: Option<F>,
    _completion: TaskCompletion,
}

impl<F: FnOnce()> ScopedBlocking<F> {
    fn run(mut self) {
        self.job.take().expect("blocking job is called once")();
    }
}

impl PluginTaskScope {
    pub fn new(plugin: &str) -> Self {
        Self::with_render_pool(plugin, crate::RenderPool::shared())
    }

    pub fn with_render_pool(plugin: &str, pool: Arc<crate::RenderPool>) -> Self {
        Self {
            plugin: plugin.to_string(),
            business_gate: nuono_app::business_runtime::BusinessGate::running(),
            activation_reason: PluginActivationReason::Enable,
            closed: std::sync::atomic::AtomicBool::new(false),
            executor: crate::runtime::RuntimeExecutor::current(),
            tasks: std::sync::Mutex::new(Vec::new()),
            async_slots: Arc::new(tokio::sync::Semaphore::new(MAX_PLUGIN_ASYNC_TASKS)),
            activity: Arc::new(TaskActivity::default()),
            cleanup: std::sync::Mutex::new(Vec::new()),
            render_pool: pool,
            image_output: crate::PluginImageOutput::default(),
            closed_notify: tokio::sync::Notify::new(),
        }
    }

    /// 在将任务作用域共享给插件前，注入宿主动态图片输出配置源。
    pub fn with_image_output(mut self, image_output: crate::PluginImageOutput) -> Self {
        self.image_output = image_output;
        self
    }

    /// 宿主创建激活作用域时指定来源。独立构造的测试作用域默认视为手动启用。
    pub fn with_activation_reason(mut self, reason: PluginActivationReason) -> Self {
        self.activation_reason = reason;
        self
    }

    /// 宿主为当前激活周期注入业务许可；普通后台任务在许可发布前不会被轮询。
    pub fn with_business_gate(mut self, gate: nuono_app::business_runtime::BusinessGate) -> Self {
        self.business_gate = gate;
        self
    }

    /// 宿主完成启用与声明发布后放行业务任务。已关闭的作用域不能重新打开。
    pub fn open_business(&self) -> bool {
        !self.is_closed() && self.business_gate.open()
    }

    /// 取消等待业务启动的任务；已运行任务由 shutdown 统一收尾。
    pub fn close_business(&self) {
        self.business_gate.close();
    }

    /// 当前作用域是否已发布业务许可且未被停用。
    pub fn business_is_running(&self) -> bool {
        !self.is_closed() && self.business_gate.is_running()
    }

    pub async fn image_output_settings(&self) -> Result<crate::ImageOutputSettings> {
        anyhow::ensure!(!self.is_closed(), "插件已停用，不能读取图片输出设置");
        let settings = self.image_output.settings().await?;
        anyhow::ensure!(!self.is_closed(), "插件已停用，不能读取图片输出设置");
        Ok(settings)
    }

    /// 生成图片的闭包在线程里执行。取消等待不会提前释放仍在运行的绘图配额；
    /// 禁用插件时会等待已经开始的工作结束，尚未开始的工作不会执行。
    pub async fn render<T, F>(&self, name: &str, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let invocation_guard = invocation::capture(&self.plugin);
        let configurations = crate::config::capture_runtime_configurations();
        let closed = self.closed_notify.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        anyhow::ensure!(!self.is_closed(), "插件已停用，不能生成图片");
        let admitted = self.render_pool.admit()?;
        let slot = tokio::select! {
            biased;
            _ = &mut closed => return Err(anyhow::anyhow!("插件已停用，绘图任务已取消")),
            permit = self.render_pool.acquire() => permit?,
        };
        let executor = self
            .executor
            .clone()
            .or_else(crate::runtime::RuntimeExecutor::current)
            .ok_or_else(|| anyhow::anyhow!("绘图需要运行中的宿主 runtime"))?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        {
            let mut tasks = self.tasks.lock().expect("锁已中毒");
            anyhow::ensure!(!self.is_closed(), "插件已停用，绘图任务已取消");
            let label = format!("{}:{name}", self.plugin);
            let dispatcher = tracing::dispatcher::get_default(Clone::clone);
            let span = tracing::info_span!("plugin_render", plugin = %self.plugin, task = name);
            let plugin = self.plugin.clone();
            let job = ScopedBlocking {
                job: Some(move || {
                    log_context::scope_blocking(Some(plugin), || {
                        invocation::run_blocking(invocation_guard, || {
                            let (_admitted, _slot) = (admitted, slot);
                            if sender.is_closed() {
                                return;
                            }
                            let result = tracing::dispatcher::with_default(&dispatcher, || {
                                let _entered = span.enter();
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                                    .unwrap_or_else(|_| {
                                        Err(anyhow::anyhow!("绘图任务 {label} 发生 panic"))
                                    })
                            });
                            // 必须先释放配额与并发许可，确保下一个渲染任务可以进入后，再通知调用方本次任务完成。
                            drop((_admitted, _slot));
                            let _ = sender.send(result);
                        })
                    });
                }),
                _completion: self.activity.register(),
            };
            let handle = executor.spawn_blocking(Box::new(move || {
                crate::config::inherit_blocking_configurations(configurations, || job.run())
            }));
            tasks.retain(|task| !task.is_finished());
            tasks.push(handle);
        }
        receiver
            .await
            .map_err(|_| anyhow::anyhow!("绘图任务已取消或宿主已退出"))?
    }

    /// 在创建此作用域的本地 runtime 上登记受管业务任务，宿主放行后才开始执行。
    ///
    /// future 必须输出 `()`；插件自己的错误处理应在 future 内部完成（记录后继续或
    /// 退出）。任务句柄由作用域跟踪，调用方无需保存。on_load/on_enable 不得等待其结果。
    pub fn spawn<F>(&self, name: &str, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if !self.try_spawn(name, fut) {
            if self.is_closed() {
                tracing::warn!(plugin = %self.plugin, task = name, "插件已停用，未启动后台任务");
            } else {
                tracing::warn!(plugin = %self.plugin, task = name, "插件后台任务容量已满或 runtime 不可用，未启动后台任务");
            }
        }
    }

    /// 启动可结束的本地初始化辅助任务，不等待业务许可。
    ///
    /// 仅用于 on_load 必须等待的准备工作。外部连接、订阅、推送和周期循环必须使用
    /// spawn；初始化任务同样受禁用、卸载和任务额度约束。
    pub fn spawn_initialization<F>(&self, name: &str, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if !self.try_spawn_initialization(name, fut) {
            tracing::warn!(plugin = %self.plugin, task = name, "初始化任务未被接纳：作用域已关闭、容量已满或 runtime 不可用");
        }
    }

    /// 尝试启动可结束的初始化辅助任务。拒绝时不执行 future。
    pub fn try_spawn_initialization<F>(&self, name: &str, fut: F) -> bool
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.spawn_scoped(name, fut)
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(any(feature = "obscura", test))]
    pub(crate) async fn cancelled(&self) {
        let closed = self.closed_notify.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if !self.is_closed() {
            closed.await;
        }
    }

    /// 持续追踪浏览器抓取任务直至其底层线程真正退出（即使调用方已提前取消等待，因为 V8 执行线程无法被 Tokio 强行中止）。
    #[cfg(any(feature = "obscura", test))]
    pub(crate) fn spawn_browser_job(&self, job: impl FnOnce() + Send + 'static) -> Result<()> {
        let executor = self
            .executor
            .clone()
            .or_else(crate::runtime::RuntimeExecutor::current)
            .ok_or_else(|| anyhow::anyhow!("浏览器需要运行中的宿主 runtime"))?;
        let mut tasks = self.tasks.lock().expect("锁已中毒");
        anyhow::ensure!(!self.is_closed(), "插件已停用，不能抓取页面");
        let invocation_guard = invocation::capture(&self.plugin);
        let configurations = crate::config::capture_runtime_configurations();
        tasks.retain(|task| !task.is_finished());
        let plugin = self.plugin.clone();
        let job = ScopedBlocking {
            job: Some(move || {
                log_context::scope_blocking(Some(plugin), || {
                    invocation::run_blocking(invocation_guard, job)
                })
            }),
            _completion: self.activity.register(),
        };
        tasks.push(executor.spawn_blocking(Box::new(move || {
            crate::config::inherit_blocking_configurations(configurations, || job.run())
        })));
        Ok(())
    }

    /// 已停用、普通后台任务达到 256 个或 runtime 不可用时返回 false。
    /// 拒绝的 future 不会执行；容量在任务 future 的资源释放后归还。
    pub fn try_spawn<F>(&self, name: &str, fut: F) -> bool
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let gate = self.business_gate.clone();
        self.spawn_scoped(name, async move {
            if gate.wait_until_running().await {
                fut.await;
            }
        })
    }

    fn spawn_scoped<F>(&self, name: &str, fut: F) -> bool
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let Some(executor) = self
            .executor
            .clone()
            .or_else(crate::runtime::RuntimeExecutor::current)
        else {
            return false;
        };
        let Ok(slot) = self.async_slots.clone().try_acquire_owned() else {
            return false;
        };
        let mut tasks = self.tasks.lock().expect("锁已中毒");
        if self.is_closed() {
            // 析构插件 future 时可能重入当前作用域，因此必须先释放 tasks 锁。
            drop(tasks);
            return false;
        }
        let plugin = self.plugin.clone();
        let log_plugin = Some(plugin.clone());
        let label = name.to_string();
        let invocation_guard = invocation::capture(&self.plugin);
        let configurations = crate::config::capture_runtime_configurations();
        // 先将插件 future 装箱（Box::pin），避免外层配置作用域、调用上下文和 catch_unwind 在每次 poll 时反复内联拷贝体积较大的 future。
        let fut = Box::pin(fut);
        let guarded = async move {
            crate::config::inherit_runtime_configurations(configurations,invocation::inherit(invocation_guard, async move {
                if let Err(panic) =
                    futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await
                {
                    let msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    tracing::warn!(plugin = %plugin, task = %label, "插件后台任务 panic: {msg}");
                }
            }))
            .await;
        };
        let guarded = ScopedFuture {
            future: Box::pin(
                tracing::instrument::WithSubscriber::with_current_subscriber(log_context::scope(
                    log_plugin, guarded,
                )),
            ),
            _slot: slot,
            _completion: self.activity.register(),
        };
        let handle = executor.spawn(Box::pin(guarded));
        tasks.retain(|h| !h.is_finished());
        tasks.push(handle);
        true
    }

    #[doc(hidden)]
    pub fn on_shutdown(&self, cleanup: impl FnOnce() + Send + 'static) {
        let _cleanup = self.activity.register();
        let mut callbacks = self.cleanup.lock().expect("锁已中毒");
        if self.is_closed() {
            drop(callbacks);
            self.run_cleanup(Box::new(cleanup));
        } else {
            callbacks.push(Box::new(cleanup));
        }
    }

    fn run_cleanup(&self, callback: Box<dyn FnOnce() + Send>) {
        if log_context::scope_blocking(Some(self.plugin.clone()), || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback))
        })
        .is_err()
        {
            tracing::warn!(plugin = %self.plugin, "插件订阅清理回调 panic，继续清理其余订阅");
        }
    }

    fn cleanup_subscriptions(&self) {
        let cleanup = std::mem::take(&mut *self.cleanup.lock().expect("锁已中毒"));
        for callback in cleanup {
            self.run_cleanup(callback);
        }
    }

    #[doc(hidden)]
    pub fn abort_all(&self) {
        self.business_gate.close();
        // 将同步清理过程也纳入活动计数屏障，防止并发调用 shutdown 时在其他线程尚未清理完订阅前就提前返回。
        let _cleanup = self.activity.register();
        {
            let tasks = self.tasks.lock().expect("锁已中毒");
            self.closed
                .store(true, std::sync::atomic::Ordering::Release);
            self.closed_notify.notify_waiters();
            for task in tasks.iter() {
                task.abort();
            }
        }
        // 清理回调可能重入当前作用域，切勿在持有 tasks 锁时执行回调。
        self.cleanup_subscriptions();
    }

    /// abort 所有受管任务并等待其资源释放，包括无法中断的已启动阻塞任务。
    /// 并发调用和取消等待不会丢失任务记录，后续调用仍等待同一个退出屏障。
    /// 受管任务内部调用会取消该任务自身；外部调用方负责等待完整退出。
    pub async fn shutdown(&self) {
        self.abort_all();
        self.activity.wait().await;
        self.cleanup_subscriptions();
        self.tasks.lock().expect("锁已中毒").clear();
    }
}

impl Drop for PluginTaskScope {
    fn drop(&mut self) {
        self.business_gate.close();
        self.cleanup_subscriptions();
        for task in self.tasks.get_mut().expect("锁已中毒").drain(..) {
            task.abort();
        }
    }
}

/// Native 插件必须实现的宿主接口定义。
///
/// # 生命周期
///
/// 准备阶段校验声明、配置和迁移，执行 `on_load` 并注册入口；宿主允许业务运行后才执行
/// `on_enable`、发布调用入口和放行受管后台任务。首次管理员设置完成前不启动业务。
/// `on_load` 绑定本地资源，`on_enable` 登记启用期任务；禁用时先停止接收事件、取消并等待受管
/// 任务退出，再调用 `on_disable`，销毁实例时调用 `on_unload`。已禁用的插件不执行迁移或初始化。
/// 实例重建和再次启用会开启新周期，`on_load` 实现应支持重复执行。
///
/// # 执行环境约定
///
/// 插件与框架统一编译，异步回调在宿主 Tokio runtime 上执行。
/// 持续运行的后台业务必须通过 `ctx.spawn` 交给生命周期管理；该任务在本插件激活完成前不会执行。
/// 初始化辅助任务使用 `tasks.spawn_initialization`，或直接 `await` 耗时可控的本地准备操作。
/// 不得在 `on_load`/`on_enable` 中等待普通 `spawn` 的结果；禁止直接调用裸 `tokio::spawn` 绕过启动与停止管理。
///
/// # 并发与错误
///
/// 回调可能在宿主持有当前插件实例锁时跨 `.await` 执行。插件不得同步重入需要修改自身
/// 生命周期的宿主操作。宿主会捕获 Rust panic 并转成错误，但已经发生的外部副作用不会回滚，
/// 插件工厂的 Rust panic 同样在实例化边界被捕获。
#[async_trait]
pub trait Plugin: Send + Sync {
    /// 全局稳定的插件标识，用于配置目录、数据库记录和工具命名空间。
    fn name(&self) -> &str;

    /// 面向用户的展示名称，不参与身份、配置目录、路由或工具命名空间。
    /// 宿主去掉首尾空白；空白名称回退到稳定的 `name()`。
    fn display_name(&self) -> &str {
        self.name()
    }

    /// 插件自身版本，与框架及 SDK 包版本分别管理。
    fn version(&self) -> &str;

    /// 面向管理员和帮助页面的简短说明。
    fn description(&self) -> &str;

    /// 插件角色分级：默认作为业务功能插件（`Feature`），底层基础设施插件应返回 `Service`。
    fn plugin_role(&self) -> PluginRole {
        PluginRole::Feature
    }

    /// 强依赖的上游插件 ID 列表：缺失或未启用时本插件无法启用，上游停用时会被拦截。
    fn required_dependencies(&self) -> Vec<&'static str> {
        vec![]
    }

    /// 可选依赖的上游插件 ID 列表：若已安装则优先于本插件加载，上游停用或缺失时允许优雅降级。
    fn optional_dependencies(&self) -> Vec<&'static str> {
        vec![]
    }

    /// 部署服务限制与扩展能力的纯声明，由宿主随启停、失败和重载统一管理。
    /// 不要在 on_load 中注册不可撤销的全局扩展点。
    fn extensions(&self) -> crate::PluginExtensions {
        crate::PluginExtensions::default()
    }

    /// 账号合并参与者：借用宿主数据库事务执行本插件的数据合并，不得独立提交事务。
    fn account_merge(
        &self,
    ) -> Option<std::sync::Arc<dyn nuono_app::account_merge::AccountMergeParticipant>> {
        None
    }

    /// 纯声明业务 schema 迁移；宿主校验完整列表并在 on_load 前事务执行未应用版本。
    /// 已发布版本的 SQL 不可修改；新增版本向前迁移，停用/卸载不会删除数据或回滚 schema。
    fn migrations(&self) -> Vec<crate::PluginMigrationForBackend> {
        vec![]
    }

    /// 当前持久清单与迁移历史分开声明；结构、资产和账号归属由注册表保留。
    /// 默认适配 CREATE-only migrations；ALTER 迁移须显式提供完整结果清单。
    fn storage_declaration(
        &self,
    ) -> nuono_error::Result<crate::storage_manifest::PluginStorageDeclaration> {
        crate::storage_manifest::PluginStorageDeclaration::from_migrations(
            self.name(),
            self.migrations(),
        )
    }

    /// 本插件拥有的业务表的准确名称，用于数据库管理目录分组。
    /// 只声明归属，不授予记录浏览、导出或修改权限；共享宿主表不要在此声明。
    fn database_tables(&self) -> Vec<String> {
        vec![]
    }

    /// 数据库管理公开的列与记录操作。仅可声明 database_tables() 中唯一归属本插件的表。
    /// 列声明允许管理员读取和导出；省略列保持保护。新增字段不会自动公开。
    /// 写操作必须维护业务约束；账务、凭据和跨表业务宜保留专用管理入口。
    fn database_management(&self) -> Vec<crate::db::DatabaseTablePolicy> {
        vec![]
    }

    /// 返回可公开展示在 `nuono状态` 卡片上的最多 4 个聚合指标。
    ///
    /// 此入口不接收用户或群身份，所有调用者看到相同的公开信息。不得返回账号、凭据、
    /// 用户记录或群私有数据。仅读取已有快照/轻量聚合；宿主有超时、并发及总预算限制，
    /// future 被取消时不得留下后台工作。空列表隐藏本插件行，默认不参与。
    async fn status_metrics(
        &self,
        _request: &crate::status::PluginStatusRequest,
    ) -> Result<Vec<crate::status::PluginStatusMetric>> {
        Ok(vec![])
    }

    /// 注入宿主资源并完成可重复的本地初始化，不连接外部服务或执行持续业务。
    async fn on_load(&mut self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }

    /// 文件配置通过校验后通知插件更新派生状态。先校验并构造新状态，再替换旧状态。
    /// 默认无操作；保留 config_handle 的插件可直接读取最新快照。
    async fn on_config_changed(&mut self, _config: &Value) -> Result<()> {
        Ok(())
    }

    /// 释放插件独占资源；代码保留在当前可执行程序中。
    async fn on_unload(&mut self) -> Result<()> {
        Ok(())
    }

    /// 登记启用期后台行为；宿主只在管理服务和首次设置完成后调用。
    /// 外部连接应在受管后台任务中建立，不能在此等待外部服务上线。
    async fn on_enable(&mut self) -> Result<()> {
        Ok(())
    }

    /// 停止启用期行为。实现应允许在进程关闭时被 best-effort 调用。
    async fn on_disable(&mut self) -> Result<()> {
        Ok(())
    }

    /// 声明插件关心的入站事件类别；未声明类别的事件不会进入 `on_event`。
    ///
    /// 默认 [`PluginEventFilter::ALL`]。只处理消息的插件应返回
    /// [`PluginEventFilter::MESSAGES`]，宿主会在派发前过滤掉 notice/request/meta，
    /// 既省掉插件里的前置判断，也减少无谓的回调。
    fn event_filter(&self) -> PluginEventFilter {
        PluginEventFilter::ALL
    }

    /// 处理普通入站事件；消息须通过命令前缀门控，notice 等仍按事件类别派发。
    async fn on_event(&self, _event: &Event) -> Result<PluginResponse> {
        Ok(PluginResponse::Continue)
    }

    /// 处理宿主已登记的交互按钮回调（宿主在调用前已完成操作人、会话作用域和当前访问权限校验）。
    /// 普通命令按钮会直接走常规命令派发链路，不会进入此回调。
    async fn on_interaction(
        &self,
        _event: &Event<'_>,
        _interaction: &crate::api::interaction::PluginInteraction,
    ) -> Result<PluginResponse> {
        Ok(PluginResponse::Continue)
    }

    /// 声明独立的自动消息监听范围，默认没有此能力。
    fn message_listener(&self) -> Option<PluginMessageListener> {
        None
    }

    /// 处理声明范围内的无前缀消息；权限、群开关和诊断与普通入口相同。
    /// 与 on_event 二选一；Continue 仍继续后续插件和触发器，返回值遵循现有传播语义。
    async fn on_message_listen(&self, _event: &Event) -> Result<PluginResponse> {
        Ok(PluginResponse::Continue)
    }

    /// 接收先前 `WaitReply` 登记会话的下一条消息。
    async fn on_message(
        &self,
        _event: &Event,
        _session_id: &str,
        _callback_id: &str,
    ) -> Result<PluginResponse> {
        Ok(PluginResponse::Continue)
    }

    /// 返回声明式非命令触发器；宿主会在插件启用时为其附加插件作用域。
    ///
    /// 这是唯一的触发器注册入口；旧的 `register_triggers` / `register_commands`
    /// 回调式注册已移除（宿主从不调用它们）。
    fn triggers(&self) -> Vec<Trigger> {
        vec![]
    }

    /// 返回声明式命令；装饰器宏收集的命令也通过此入口注册。
    fn commands(&self) -> Vec<Command> {
        vec![]
    }

    /// 声明插件 Web 路由。路径相对于 `/api/plugin/{plugin_name}`，权限默认必须显式选择；
    /// 未声明的请求由宿主按 `ManageRuntime` 处理。
    fn web_routes(&self) -> Vec<PluginWebRoute> {
        vec![]
    }

    /// 声明嵌入式、独立构建的 WebUI 页面，每次激活时收集一次。
    fn web_pages(&self) -> Vec<PluginWebPage> {
        vec![]
    }

    /// 返回 Agent 工具；宿主会将名称改写为 `plugin.<plugin>.<tool>`。
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![]
    }

    /// 声明按需读取的 Agent 工作流。宿主注册只读的 `plugin.<plugin>.skill.<id>` 工具，
    /// 禁用或卸载时与普通工具一起移除；正文不改变自主等级、本人身份或审批要求。
    fn agent_skills(&self) -> Vec<PluginAgentSkill> {
        vec![]
    }

    fn service_config(&self) -> Plugins {
        Plugins::new(self.name())
    }

    /// 声明本插件的 secret 字段（token、外部账号凭据等）。
    ///
    /// 字段 key 是插件内唯一的名字；宿主把它规范化为 `plugin.<plugin_name>.<key>` 存进
    /// `runtime_secrets`，并通过 [`PluginContext::secrets`] 以本地 key 供插件存取。
    /// 未声明字段的读写删会被拒绝。空值、非法字符或重复字段会在加载阶段拒绝本插件。
    fn secret_fields(&self) -> Vec<PluginSecretField> {
        vec![]
    }

    /// 唯一业务配置声明入口；默认值、表单和校验从字段派生。无配置时返回空列表。
    fn config_fields(&self) -> Vec<PluginConfigField> {
        vec![]
    }

    /// 声明结构化配置资源（由插件自定义校验逻辑、支持按资源独立 CAS 并发更新的复杂配置项）。
    fn config_resources(
        &self,
    ) -> Vec<nuono_app::configuration_resources::ConfigurationResourceDefinition> {
        vec![]
    }

    /// 持久化业务配置的数据格式版本号（独立于插件 crate 的发布版本号）。
    fn config_version(&self) -> i64 {
        1
    }

    /// 在插件激活前，将旧版全局配置与结构化配置资源一并迁移至当前配置版本。
    fn config_migration(
        &self,
    ) -> Option<Arc<dyn nuono_app::config_schema::ConfigurationMigration>> {
        None
    }

    async fn handle_web_request(&self, _req: &PluginWebRequest) -> PluginWebResponse {
        PluginWebResponse::NotFound
    }
}

#[cfg(test)]
mod task_scope_tests {
    #[tokio::test]
    async fn abort_all_removes_subscriptions_without_locking_out_reentrant_cleanup() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let scope = Arc::new(super::PluginTaskScope::new("abort-cleanup"));
        let completed = Arc::new(AtomicUsize::new(0));
        let weak = Arc::downgrade(&scope);
        let count = completed.clone();
        scope.on_shutdown(move || {
            assert!(!weak.upgrade().unwrap().try_spawn("closed", async {}));
            count.fetch_add(1, Ordering::SeqCst);
        });
        scope.on_shutdown(|| panic!("cleanup probe"));
        let count = completed.clone();
        scope.on_shutdown(move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        scope.abort_all();
        scope.abort_all();
        scope.shutdown().await;
        assert_eq!(completed.load(Ordering::SeqCst), 2);
    }

    use super::PluginTaskScope;
    use std::sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test]
    async fn shutdown_reaps_cleanup_registered_by_a_cancelled_task() {
        struct RegisterOnDrop(Weak<PluginTaskScope>, Arc<AtomicBool>);
        impl Drop for RegisterOnDrop {
            fn drop(&mut self) {
                if let Some(scope) = self.0.upgrade() {
                    let cleaned = self.1.clone();
                    scope.on_shutdown(move || cleaned.store(true, Ordering::SeqCst));
                }
            }
        }
        let scope = Arc::new(PluginTaskScope::new("shutdown-race"));
        let cleaned = Arc::new(AtomicBool::new(false));
        let guard = RegisterOnDrop(Arc::downgrade(&scope), cleaned.clone());
        let (started, ready) = tokio::sync::oneshot::channel();
        scope.spawn("register-at-cancellation", async move {
            let _guard = guard;
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        scope.shutdown().await;
        assert!(cleaned.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
#[path = "../../tests/unit/plugin/task_scope_fault_tests.rs"]
mod task_scope_fault_tests;

#[cfg(test)]
mod business_task_tests {
    use super::PluginTaskScope;
    use nuono_app::business_runtime::BusinessGate;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test]
    async fn initialization_can_finish_while_business_waits_for_publication() {
        let gate = BusinessGate::paused();
        let scope = PluginTaskScope::new("startup-order").with_business_gate(gate);
        let (business_tx, mut business_rx) = tokio::sync::oneshot::channel();
        scope.spawn("business", async move {
            let _ = business_tx.send(());
        });
        let (initial_tx, initial_rx) = tokio::sync::oneshot::channel();
        scope.spawn_initialization("local-prepare", async move {
            let _ = initial_tx.send(());
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), initial_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            business_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(scope.open_business());
        tokio::time::timeout(std::time::Duration::from_secs(1), business_rx)
            .await
            .unwrap()
            .unwrap();
        scope.shutdown().await;
    }

    #[tokio::test]
    async fn disabling_before_start_drops_queued_work_without_polling_it() {
        struct Release(Arc<AtomicBool>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let scope =
            PluginTaskScope::new("cancel-before-start").with_business_gate(BusinessGate::paused());
        let ran = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let captured = Release(released.clone());
        let task_ran = ran.clone();
        scope.spawn("must-not-run", async move {
            let _captured = captured;
            task_ran.store(true, Ordering::SeqCst);
        });
        scope.shutdown().await;
        assert!(!ran.load(Ordering::SeqCst));
        assert!(released.load(Ordering::SeqCst));
        assert!(!scope.open_business());
        assert!(!scope.try_spawn_initialization("closed", async {}));
    }
}
