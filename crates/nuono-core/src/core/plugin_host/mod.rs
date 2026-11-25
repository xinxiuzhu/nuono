//! 源码插件的进程内宿主与生命周期编排。
//!
//! 宿主负责把插件实例、数据库启用状态、配置 schema、触发器和 Agent 工具同步到
//! 同一生命周期。集合仅保存实例句柄；业务回调使用可撤销的调用批次号和单插件锁。
//! 同步阻塞或不让出执行权的插件无法由异步取消抢占，插件仍与宿主共享进程。

use futures::FutureExt;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, instrument, warn};

use crate::agent::{
    AgentContext, AgentTool, AgentToolContext, BuiltinToolRegistry, ToolParameter, ToolRegistry,
    ToolRiskLevel, ToolScheduling,
};
use crate::api::waiter::WaitManager;
use crate::config::schema::ConfigValue;
use crate::core::event_bus::EventBus;
use crate::core::trigger_registry::TriggerRegistry;
use crate::core::{
    cooldown::CooldownTracker,
    framework_control::FrameworkControl,
    permission::{ActorPermission, PermissionLevel, PermissionManager},
    plugin_service::{PluginArea, Plugins},
    stats::CommandStatsManager,
};
use crate::db::Database;
use crate::error::Result;
use crate::plugin::{
    Event, LoadedPlugin, PluginActivationReason, PluginContext, PluginResponse, PluginTaskScope,
};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct PluginFilterContext {
    pub user_id: String,
    pub group_id: Option<String>,
    pub msg_type: String,
    pub actor_permission: ActorPermission,
}

pub struct PluginProcessResult {
    pub plugin_name: Option<String>,
    pub response: PluginResponse,
    pub(crate) wait_invocation: Option<PluginInvocation>,
}

impl PluginProcessResult {
    fn new(plugin_name: Option<String>, response: PluginResponse) -> Self {
        Self {
            plugin_name,
            response,
            wait_invocation: None,
        }
    }

    fn with_wait_invocation(mut self, invocation: PluginInvocation) -> Self {
        self.wait_invocation = Some(invocation);
        self
    }

    fn continue_without_plugin() -> Self {
        Self::new(None, PluginResponse::Continue)
    }
}

struct PluginScopedTool {
    scoped_name: String,
    inner: Arc<dyn AgentTool>,
    diagnostics: Option<Arc<diagnostics::PluginMetrics>>,
    epoch: Option<InvocationEpoch>,
}

#[async_trait::async_trait]
impl AgentTool for PluginScopedTool {
    fn name(&self) -> &str {
        &self.scoped_name
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn risk_level(&self) -> ToolRiskLevel {
        self.inner.risk_level()
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
    }

    fn parameters(&self) -> Vec<ToolParameter> {
        self.inner.parameters()
    }

    fn input_schema(&self) -> Option<Value> {
        self.inner.input_schema()
    }

    fn scheduling(&self, ctx: &AgentContext, input: &Value) -> ToolScheduling {
        self.inner.scheduling(ctx, input)
    }

    async fn call(
        &self,
        ctx: AgentToolContext,
        input: serde_json::Value,
    ) -> crate::error::Result<serde_json::Value> {
        let call = async {
            match &self.diagnostics {
                Some(metrics) => {
                    metrics
                        .observe(&self.scoped_name, self.inner.call(ctx, input))
                        .await
                }
                None => self.inner.call(ctx, input).await,
            }
        };
        match &self.epoch {
            Some(epoch) => epoch.run(call).await.unwrap_or_else(|| {
                Err(crate::error::NuonoError::Conflict(
                    "插件调用代次已撤销".into(),
                ))
            }),
            None => call.await,
        }
    }

    async fn call_with_media(
        &self,
        ctx: AgentToolContext,
        input: serde_json::Value,
    ) -> crate::error::Result<nuono_app::agent::tool::AgentToolOutput> {
        let call = async {
            match &self.diagnostics {
                Some(metrics) => {
                    metrics
                        .observe(&self.scoped_name, self.inner.call_with_media(ctx, input))
                        .await
                }
                None => self.inner.call_with_media(ctx, input).await,
            }
        };
        match &self.epoch {
            Some(epoch) => epoch.run(call).await.unwrap_or_else(|| {
                Err(crate::error::NuonoError::Conflict(
                    "插件调用代次已撤销".into(),
                ))
            }),
            None => call.await,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginInfoView {
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub plugin_role: nuono_app::plugin_role::PluginRole,
    pub required_dependencies: Vec<String>,
    pub optional_dependencies: Vec<String>,
    pub message_listener: Option<crate::plugin::PluginMessageListener>,
    pub enabled: bool,
    pub config_schema: serde_json::Value,
}

impl Default for PluginInfoView {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            version: String::new(),
            description: "未知插件".into(),
            plugin_role: nuono_app::plugin_role::PluginRole::Feature,
            required_dependencies: Vec::new(),
            optional_dependencies: Vec::new(),
            message_listener: None,
            enabled: false,
            config_schema: serde_json::json!({}),
        }
    }
}

pub(crate) fn plugin_display_name(name: &str, display_name: &str) -> String {
    let display_name = display_name.trim();
    if display_name.is_empty() {
        name.to_string()
    } else {
        display_name.to_string()
    }
}

/// 插件实例、声明式扩展和运行时状态的所有者。
///
/// 插件按加载顺序存储并按逆序关闭。集合锁仅保护句柄快照，生命周期按插件名串行；
/// 不同插件可并行执行，关闭屏障阻止退出期间发布新调用批次。
pub struct PluginHost {
    platform_role_authority: nuono_app::configuration::PlatformRoleAuthority,
    platform_role_verifiers:
        Arc<RwLock<Vec<Arc<dyn nuono_app::configuration::PlatformRoleVerifier>>>>,
    extensions: nuono_app::plugin_extensions::PluginExtensionScope,
    pub locales: nuono_app::localization::LocaleService,
    image_output: nuono_plugin::image_output::PluginImageOutput,
    db: Database,
    event_bus: Arc<EventBus>,
    permission_manager: Arc<PermissionManager>,
    paths: crate::core::paths::Paths,
    plugins: RwLock<Vec<HostedPlugin>>,
    lifecycle_operations:
        std::sync::Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    lifecycle_admission: RwLock<()>,
    closing: std::sync::atomic::AtomicBool,
    business_running: std::sync::atomic::AtomicBool,
    business_publication: std::sync::Mutex<()>,
    pending_activations: RwLock<HashMap<String, business::PendingActivation>>,
    registration_writer: tokio::sync::Mutex<()>,
    /// 本程序发现的插件目录；与可用实例、启停状态和历史数据库记录分离。
    installed_plugins: RwLock<HashMap<String, PluginInfoView>>,
    /// 完整目录清理是每个宿主的一次性启动操作，包括明确的零插件宿主。
    inventory_reconciliation_started: std::sync::atomic::AtomicBool,
    trigger_registry: Arc<TriggerRegistry>,
    plugin_registry: RwLock<HashMap<String, Arc<Plugins>>>,
    pub cooldown_tracker: Arc<CooldownTracker>,
    pub command_stats: Arc<CommandStatsManager>,
    pub wait_manager: Arc<WaitManager>,
    tool_registry: Arc<ToolRegistry>,
    agent_runtime: Arc<std::sync::RwLock<Option<crate::agent::AgentRuntime>>>,
    pub framework_control: FrameworkControl,
    action_router: Arc<crate::core::RuntimeActionRouter>,
    config_manager: RwLock<Option<Arc<crate::config::PluginConfigManager>>>,

    /// 每个插件当前 `on_load` 周期的受管后台任务作用域。
    ///
    /// 生命周期边界（禁用、卸载、实例重建）由宿主统一 abort+await，见
    /// [`PluginHost::reap_plugin_tasks`]。
    task_scopes: RwLock<HashMap<String, Arc<PluginTaskScope>>>,

    pending_wait_callbacks: RwLock<HashMap<String, String>>,

    /// 每个已加载插件声明的 secret 字段 schema（按插件名）。
    ///
    /// 加载阶段注册；插件禁用不清空（secret 仍在 DB，仍需可列出），卸载/实例重建随当前
    /// 插件更新。这里只存字段 schema，不缓存任何 secret 值。
    secret_schemas: RwLock<HashMap<String, Vec<crate::plugin::PluginSecretField>>>,
    web_routes: RwLock<HashMap<String, Vec<crate::plugin::PluginWebRoute>>>,
    web_pages: RwLock<HashMap<String, web_pages::RegisteredPages>>,
    web_page_generation: std::sync::atomic::AtomicU64,
    config_declarations: RwLock<HashMap<String, Vec<crate::core::PluginConfigField>>>,
    config_revisions: RwLock<HashMap<String, String>>,
    diagnostics: diagnostics::DiagnosticsRegistry,
    /// 诊断策略读入和持久化更新共享序列，提交成功才发布 metrics 快照。
    diagnostic_policy_writer: tokio::sync::Mutex<()>,
    trigger_control: Arc<trigger_control::TriggerControl>,
    trigger_policy_writer: tokio::sync::Mutex<()>,
    interactions: interactions::InteractionRegistry,
    /// 进程内类型化插件服务注册表；随插件停用、失败或关闭自动回收。
    service_registry: crate::plugin::PluginServiceRegistry,
}

/// 把激活失败的主错误与清理阶段的错误合并成一个可观察的 [`crate::error::NuonoError`]。
fn activation_failed_error(
    name: &str,
    primary: impl Into<String>,
    cleanup: crate::error::Result<()>,
) -> crate::error::NuonoError {
    let mut reason = primary.into();
    if let Err(ce) = cleanup {
        reason = format!("{reason}；fail-closed 清理未完全成功: {ce}");
    }
    crate::error::NuonoError::PluginExecution {
        plugin: name.to_string(),
        reason,
    }
}

mod agent_skills;
mod config;
mod config_updates;
mod configuration_access;
mod diagnostics;
mod dispatch;
pub use diagnostics::{
    MAX_SLOW_PLUGIN_THRESHOLD_MS, PluginDiagnostics, PluginSlowCall, SLOW_PLUGIN_THRESHOLD_MS,
};
mod account_conversations;
mod business;
mod embeddings;
mod hosted;
use hosted::{
    ACTIVATION_TIMEOUT, ActivationGuard, HostedPlugin, InvocationEpoch, LIFECYCLE_TIMEOUT,
    PluginInvocation,
};
mod install;
pub(crate) mod interactions;
mod inventory;
mod lifecycle;
mod loading;
mod message_route;
pub use message_route::PluginMessageRoute;
pub(crate) mod trigger_control;
pub use nuono_storage::plugins::PluginTriggerPolicy;

use loading::PluginDeclaration;
#[cfg(test)]
use loading::{
    single_reload_candidate, validated_plugin_secret_fields, validated_plugin_web_routes,
};
pub(crate) mod image_output;
mod registry;
mod route_pattern;
mod web_pages;
pub use web_pages::PluginWebPageView;
mod runtime;
mod status;
pub use status::{MAX_PLUGIN_STATUS_ROWS, PluginStatusAvailability, PluginStatusRow};
#[cfg(test)]
#[path = "../../../tests/unit/core/plugin_host/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/core/plugin_host/service_and_dependency_tests.rs"]
mod service_and_dependency_tests;

#[cfg(test)]
#[path = "../../../../../xtask/fixtures/source_plugin.rs"]
mod sdk_fixture;

impl PluginHost {
    pub(crate) fn paths(&self) -> &crate::core::Paths {
        &self.paths
    }
}

impl PluginHost {
    pub async fn update_deployment_policy(
        &self,
        actor: i64,
        providers: &[String],
        platform_roles_enabled: bool,
    ) -> Result<()> {
        let mut transitional = self.db.required_service_providers().await?;
        transitional.extend_from_slice(providers);
        transitional.sort();
        transitional.dedup();
        self.extensions.require_providers(&transitional);
        let result = self
            .db
            .set_deployment_policy(actor, providers, platform_roles_enabled)
            .await;
        // 刷新失败时继续维持更严格的过渡期封闭门控（fail-closed），直到管理员修复或重启。
        self.refresh_required_service_providers().await?;
        if result.is_ok() {
            self.platform_role_authority
                .set_enabled(platform_roles_enabled);
        }
        result
    }
    pub async fn refresh_required_service_providers(&self) -> Result<()> {
        let providers = self.db.required_service_providers().await?;
        self.extensions.require_providers(&providers);
        Ok(())
    }
    pub async fn register_platform_role_verifier(
        &self,
        provider: Arc<dyn nuono_app::configuration::PlatformRoleVerifier>,
    ) {
        let mut providers = self.platform_role_verifiers.write().await;
        providers.retain(|current| current.provider_id() != provider.provider_id());
        providers.push(provider);
        self.platform_role_authority.invalidate();
    }
    pub fn invalidate_platform_role_proofs(&self) {
        self.platform_role_authority.invalidate();
    }

    /// 返回宿主共享的 Agent 工具注册表，供管理接口枚举已注册的内置与插件工具。
    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        Arc::clone(&self.tool_registry)
    }

    /// 返回宿主托管的插件服务注册表。
    pub fn service_registry(&self) -> &crate::plugin::PluginServiceRegistry {
        &self.service_registry
    }

    /// 查询指定插件的角色分级；未登记时默认视为业务功能插件。
    pub async fn plugin_role(&self, name: &str) -> nuono_app::plugin_role::PluginRole {
        if let Some(info) = self.installed_plugins.read().await.get(name) {
            return info.plugin_role;
        }
        let plugins = self.plugins.read().await.clone();
        if let Some(hosted) = plugins.iter().find(|plugin| plugin.name() == name)
            && let Ok(plugin) = hosted.plugin.try_read()
        {
            return plugin.plugin_role();
        }
        self.db
            .plugin_policy(name)
            .await
            .ok()
            .and_then(|row| nuono_app::plugin_role::PluginRole::parse(&row.plugin_role))
            .unwrap_or_default()
    }

    /// 查询指定插件声明的强依赖与可选依赖列表。
    pub async fn plugin_dependencies(&self, name: &str) -> (Vec<String>, Vec<String>) {
        if let Some(info) = self.installed_plugins.read().await.get(name) {
            return (
                info.required_dependencies.clone(),
                info.optional_dependencies.clone(),
            );
        }
        (Vec::new(), Vec::new())
    }
    pub async fn configuration_actor_for_group(
        &self,
        caller: &nuono_app::auth::CallerIdentity,
        group_id: i64,
    ) -> Result<nuono_app::configuration::ConfigurationActor> {
        configuration_access::actor_for_group(
            &self.db,
            &self.platform_role_authority,
            &self.platform_role_verifiers,
            caller,
            group_id,
        )
        .await
    }

    /// 批量判定调用者在哪些候选群上具备已验证的平台管理员身份。
    ///
    /// 供 `GET /api/groups` 等列表接口复用，避免逐群重复查全局开关、用户身份与 verifier 列表。
    pub async fn verified_platform_role_groups(
        &self,
        caller: &nuono_app::auth::CallerIdentity,
        groups: &[nuono_storage::groups::Group],
    ) -> Result<std::collections::HashSet<i64>> {
        configuration_access::verified_platform_role_groups(
            &self.db,
            &self.platform_role_authority,
            &self.platform_role_verifiers,
            caller,
            groups,
        )
        .await
    }
}
