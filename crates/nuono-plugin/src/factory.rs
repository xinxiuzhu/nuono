//! 两段式插件生命周期契约：静态描述 ([`PluginDescriptor`]) + 运行期挂载 ([`PluginFactory::mount`])。
//!
//! 将“静态元数据声明”与“持有 [`PluginContext`] 的运行期实例”彻底分离，
//! 从类型系统层面消灭 `OnceLock<Database>` 与未初始化状态检查。

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::agent::AgentTool;
use crate::auth::CapabilityProvider;
use crate::core::command::Command;
use crate::core::config_schema::PluginConfigField;
use crate::core::plugin_service::Plugins;
use crate::core::trigger::Trigger;
use crate::core_seams::ServiceEntitlement;
use crate::db::DatabaseTablePolicy;
use crate::plugin::{
    Event, Plugin, PluginAgentSkill, PluginContext, PluginEventFilter, PluginMessageListener,
    PluginResponse, PluginRole, PluginSecretField, PluginWebPage, PluginWebRequest,
    PluginWebResponse, PluginWebRoute,
};
use crate::status::{PluginStatusMetric, PluginStatusRequest};
use crate::storage_manifest::PluginStorageDeclaration;
use crate::web::ErasedWebRouter;
use crate::{AccountMergeParticipant, PluginExtensions, PluginMigrationForBackend};

/// 阶段 1：纯同步、无副作用的插件静态声明。
///
/// 宿主在执行数据库迁移、构造 [`PluginContext`] 和校验部署策略前读取此描述符。
#[non_exhaustive]
pub struct PluginDescriptor {
    pub id: String,
    pub display_name: Option<String>,
    pub version: String,
    pub description: String,
    pub plugin_role: PluginRole,
    pub required_dependencies: Vec<&'static str>,
    pub optional_dependencies: Vec<&'static str>,
    pub config_fields: Vec<PluginConfigField>,
    pub config_resources: Vec<nuono_app::configuration_resources::ConfigurationResourceDefinition>,
    pub config_version: i64,
    pub config_migration: Option<Arc<dyn nuono_app::config_schema::ConfigurationMigration>>,
    pub secret_fields: Vec<PluginSecretField>,
    pub migrations: Vec<PluginMigrationForBackend>,
    pub storage_declaration: Option<PluginStorageDeclaration>,
    pub database_tables: Vec<String>,
    pub database_management: Vec<DatabaseTablePolicy>,
    pub service_config: Option<Plugins>,
    pub event_filter: PluginEventFilter,
    pub message_listener: Option<PluginMessageListener>,
    pub web_pages: Vec<PluginWebPage>,
    pub web_routes: Vec<PluginWebRoute>,
    pub protects_services: bool,
    pub capabilities: Option<Arc<dyn CapabilityProvider>>,
    pub account_merge: Option<Arc<dyn AccountMergeParticipant>>,
}

impl std::fmt::Debug for PluginDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginDescriptor")
            .field("id", &self.id)
            .field("display_name", &self.display_name)
            .field("version", &self.version)
            .field("description", &self.description)
            .field("config_version", &self.config_version)
            .field("protects_services", &self.protects_services)
            .finish_non_exhaustive()
    }
}

impl PluginDescriptor {
    /// 构造必填标识、版本和描述的基础描述符。
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            version: version.into(),
            description: description.into(),
            plugin_role: PluginRole::Feature,
            required_dependencies: Vec::new(),
            optional_dependencies: Vec::new(),
            config_fields: Vec::new(),
            config_resources: Vec::new(),
            config_version: 1,
            config_migration: None,
            secret_fields: Vec::new(),
            migrations: Vec::new(),
            storage_declaration: None,
            database_tables: Vec::new(),
            database_management: Vec::new(),
            service_config: None,
            event_filter: PluginEventFilter::ALL,
            message_listener: None,
            web_pages: Vec::new(),
            web_routes: Vec::new(),
            protects_services: false,
            capabilities: None,
            account_merge: None,
        }
    }

    pub fn display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    pub fn plugin_role(mut self, role: PluginRole) -> Self {
        self.plugin_role = role;
        self
    }

    pub fn required_dependencies(
        mut self,
        dependencies: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        self.required_dependencies = dependencies.into_iter().collect();
        self
    }

    pub fn optional_dependencies(
        mut self,
        dependencies: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        self.optional_dependencies = dependencies.into_iter().collect();
        self
    }

    pub fn config_fields(mut self, fields: Vec<PluginConfigField>) -> Self {
        self.config_fields = fields;
        self
    }

    pub fn config_resources(
        mut self,
        resources: Vec<nuono_app::configuration_resources::ConfigurationResourceDefinition>,
    ) -> Self {
        self.config_resources = resources;
        self
    }

    pub fn config_version(mut self, version: i64) -> Self {
        self.config_version = version;
        self
    }

    pub fn config_migration(
        mut self,
        migration: Arc<dyn nuono_app::config_schema::ConfigurationMigration>,
    ) -> Self {
        self.config_migration = Some(migration);
        self
    }

    pub fn secret_fields(mut self, fields: Vec<PluginSecretField>) -> Self {
        self.secret_fields = fields;
        self
    }

    pub fn migrations(mut self, migrations: Vec<PluginMigrationForBackend>) -> Self {
        self.migrations = migrations;
        self
    }

    pub fn storage_declaration(mut self, declaration: PluginStorageDeclaration) -> Self {
        self.storage_declaration = Some(declaration);
        self
    }

    pub fn database_tables(mut self, tables: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.database_tables = tables.into_iter().map(Into::into).collect();
        self
    }

    pub fn database_management(mut self, policies: Vec<DatabaseTablePolicy>) -> Self {
        self.database_management = policies;
        self
    }

    pub fn service_config(mut self, config: Plugins) -> Self {
        self.service_config = Some(config);
        self
    }

    pub fn event_filter(mut self, filter: PluginEventFilter) -> Self {
        self.event_filter = filter;
        self
    }

    pub fn message_listener(mut self, listener: PluginMessageListener) -> Self {
        self.message_listener = Some(listener);
        self
    }

    pub fn web_pages(mut self, pages: Vec<PluginWebPage>) -> Self {
        self.web_pages = pages;
        self
    }

    pub fn web_routes(mut self, routes: Vec<PluginWebRoute>) -> Self {
        self.web_routes = routes;
        self
    }

    /// 静态声明本插件提供部署级服务权益策略（确保加载失败或尚未完成挂载时严格 fail-closed）。
    pub fn protects_services(mut self, required: bool) -> Self {
        self.protects_services = required;
        self
    }

    pub fn capabilities(mut self, provider: Arc<dyn CapabilityProvider>) -> Self {
        self.capabilities = Some(provider);
        self
    }

    pub fn account_merge(mut self, participant: Arc<dyn AccountMergeParticipant>) -> Self {
        self.account_merge = Some(participant);
        self
    }
}

/// 挂载后插件的可选生命周期与事件回调契约。
#[async_trait]
pub trait MountedLifecycle: Send + Sync {
    async fn on_enable(&mut self) -> Result<()> {
        Ok(())
    }

    async fn on_disable(&mut self) -> Result<()> {
        Ok(())
    }

    async fn on_unload(&mut self) -> Result<()> {
        Ok(())
    }

    async fn on_config_changed(&mut self, _config: &Value) -> Result<()> {
        Ok(())
    }

    async fn on_event(&self, _event: &Event) -> Result<PluginResponse> {
        Ok(PluginResponse::Continue)
    }

    async fn on_interaction(
        &self,
        _event: &Event<'_>,
        _interaction: &crate::api::interaction::PluginInteraction,
    ) -> Result<PluginResponse> {
        Ok(PluginResponse::Continue)
    }

    async fn on_message_listen(&self, _event: &Event) -> Result<PluginResponse> {
        Ok(PluginResponse::Continue)
    }

    async fn on_message(
        &self,
        _event: &Event,
        _session_id: &str,
        _callback_id: &str,
    ) -> Result<PluginResponse> {
        Ok(PluginResponse::Continue)
    }

    async fn status_metrics(
        &self,
        _request: &PluginStatusRequest,
    ) -> Result<Vec<PluginStatusMetric>> {
        Ok(Vec::new())
    }
}

/// 阶段 2 产物：由 [`PluginFactory::mount`] 基于已就绪的 [`PluginContext`] 构造的运行期插件。
#[non_exhaustive]
pub struct MountedPlugin {
    pub commands: Vec<Command>,
    pub triggers: Vec<Trigger>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub agent_skills: Vec<PluginAgentSkill>,
    pub web_router: Option<ErasedWebRouter>,
    pub service_entitlement: Option<Arc<dyn ServiceEntitlement>>,
    pub capabilities: Option<Arc<dyn CapabilityProvider>>,
    pub lifecycle: Option<Box<dyn MountedLifecycle>>,
}

impl std::fmt::Debug for MountedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountedPlugin")
            .field("command_count", &self.commands.len())
            .field("trigger_count", &self.triggers.len())
            .field("tool_count", &self.tools.len())
            .field("skill_count", &self.agent_skills.len())
            .field("has_web_router", &self.web_router.is_some())
            .field("has_lifecycle", &self.lifecycle.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for MountedPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl MountedPlugin {
    /// 创建空的运行期挂载对象。
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            triggers: Vec::new(),
            tools: Vec::new(),
            agent_skills: Vec::new(),
            web_router: None,
            service_entitlement: None,
            capabilities: None,
            lifecycle: None,
        }
    }

    pub fn with_commands(mut self, commands: Vec<Command>) -> Self {
        self.commands = commands;
        self
    }

    pub fn with_triggers(mut self, triggers: Vec<Trigger>) -> Self {
        self.triggers = triggers;
        self
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_agent_skills(mut self, skills: Vec<PluginAgentSkill>) -> Self {
        self.agent_skills = skills;
        self
    }

    pub fn with_web_router(mut self, router: ErasedWebRouter) -> Self {
        self.web_router = Some(router);
        self
    }

    pub fn with_service_entitlement(mut self, entitlement: Arc<dyn ServiceEntitlement>) -> Self {
        self.service_entitlement = Some(entitlement);
        self
    }

    pub fn with_capabilities(mut self, capabilities: Arc<dyn CapabilityProvider>) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    pub fn with_lifecycle(mut self, lifecycle: impl MountedLifecycle + 'static) -> Self {
        self.lifecycle = Some(Box::new(lifecycle));
        self
    }
}

/// 两段式插件工厂 Trait。
///
/// 1. [`Self::describe`] 纯同步声明插件静态元数据，宿主据此执行迁移并装配 [`PluginContext`]；
/// 2. [`Self::mount`] 接收已就绪的完整 [`PluginContext`]，直接构造强类型状态、路由、指令和扩展，
///    无需 `OnceLock` 或 `Option<Database>` 防御分支。
#[async_trait]
pub trait PluginFactory: Send + Sync + 'static {
    fn describe(&self) -> PluginDescriptor;
    async fn mount(&self, ctx: PluginContext) -> Result<MountedPlugin>;
}

/// 将 [`PluginFactory`] 桥接到宿主 [`Plugin`] 契约的适配器。
pub struct FactoryPlugin<F: PluginFactory> {
    factory: F,
    descriptor: PluginDescriptor,
    mounted: Option<MountedPlugin>,
}

impl<F: PluginFactory> std::fmt::Debug for FactoryPlugin<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FactoryPlugin")
            .field("descriptor", &self.descriptor)
            .field("mounted", &self.mounted)
            .finish_non_exhaustive()
    }
}

impl<F: PluginFactory> FactoryPlugin<F> {
    /// 构造适配器并缓存一次静态描述符。
    pub fn new(factory: F) -> Self {
        let descriptor = factory.describe();
        Self {
            factory,
            descriptor,
            mounted: None,
        }
    }
}

#[async_trait]
impl<F: PluginFactory> Plugin for FactoryPlugin<F> {
    fn name(&self) -> &str {
        &self.descriptor.id
    }

    fn display_name(&self) -> &str {
        self.descriptor
            .display_name
            .as_deref()
            .unwrap_or(&self.descriptor.id)
    }

    fn version(&self) -> &str {
        &self.descriptor.version
    }

    fn description(&self) -> &str {
        &self.descriptor.description
    }

    fn plugin_role(&self) -> PluginRole {
        self.descriptor.plugin_role
    }

    fn required_dependencies(&self) -> Vec<&'static str> {
        self.descriptor.required_dependencies.clone()
    }

    fn optional_dependencies(&self) -> Vec<&'static str> {
        self.descriptor.optional_dependencies.clone()
    }

    fn extensions(&self) -> PluginExtensions {
        let mut ext =
            PluginExtensions::new().with_service_policy_required(self.descriptor.protects_services);
        if let Some(capabilities) = &self.descriptor.capabilities {
            ext = ext.with_capabilities(capabilities.clone());
        }
        if let Some(mounted) = &self.mounted {
            if let Some(entitlement) = &mounted.service_entitlement {
                ext = ext.with_service_entitlement(entitlement.clone());
            }
            if let Some(capabilities) = &mounted.capabilities {
                ext = ext.with_capabilities(capabilities.clone());
            }
        }
        ext
    }

    fn account_merge(&self) -> Option<Arc<dyn AccountMergeParticipant>> {
        self.descriptor.account_merge.clone()
    }

    fn migrations(&self) -> Vec<PluginMigrationForBackend> {
        self.descriptor.migrations.clone()
    }

    fn storage_declaration(&self) -> nuono_error::Result<PluginStorageDeclaration> {
        if let Some(declaration) = &self.descriptor.storage_declaration {
            return Ok(declaration.clone());
        }
        PluginStorageDeclaration::from_migrations(
            &self.descriptor.id,
            self.descriptor.migrations.clone(),
        )
    }

    fn database_tables(&self) -> Vec<String> {
        self.descriptor.database_tables.clone()
    }

    fn database_management(&self) -> Vec<DatabaseTablePolicy> {
        self.descriptor.database_management.clone()
    }

    fn config_fields(&self) -> Vec<PluginConfigField> {
        self.descriptor.config_fields.clone()
    }

    fn config_resources(
        &self,
    ) -> Vec<nuono_app::configuration_resources::ConfigurationResourceDefinition> {
        self.descriptor.config_resources.clone()
    }

    fn config_version(&self) -> i64 {
        self.descriptor.config_version
    }

    fn config_migration(
        &self,
    ) -> Option<Arc<dyn nuono_app::config_schema::ConfigurationMigration>> {
        self.descriptor.config_migration.clone()
    }

    fn secret_fields(&self) -> Vec<PluginSecretField> {
        self.descriptor.secret_fields.clone()
    }

    fn service_config(&self) -> Plugins {
        self.descriptor
            .service_config
            .clone()
            .unwrap_or_else(|| Plugins::new(&self.descriptor.id))
    }

    fn event_filter(&self) -> PluginEventFilter {
        self.descriptor.event_filter
    }

    fn message_listener(&self) -> Option<PluginMessageListener> {
        self.descriptor.message_listener.clone()
    }

    fn web_pages(&self) -> Vec<PluginWebPage> {
        self.descriptor.web_pages.clone()
    }

    fn web_routes(&self) -> Vec<PluginWebRoute> {
        if let Some(router) = self
            .mounted
            .as_ref()
            .and_then(|mounted| mounted.web_router.as_ref())
        {
            return router.routes();
        }
        self.descriptor.web_routes.clone()
    }

    fn commands(&self) -> Vec<Command> {
        self.mounted
            .as_ref()
            .map(|mounted| mounted.commands.clone())
            .unwrap_or_default()
    }

    fn triggers(&self) -> Vec<Trigger> {
        self.mounted
            .as_ref()
            .map(|mounted| mounted.triggers.clone())
            .unwrap_or_default()
    }

    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.mounted
            .as_ref()
            .map(|mounted| mounted.tools.clone())
            .unwrap_or_default()
    }

    fn agent_skills(&self) -> Vec<PluginAgentSkill> {
        self.mounted
            .as_ref()
            .map(|mounted| mounted.agent_skills.clone())
            .unwrap_or_default()
    }

    async fn on_load(&mut self, ctx: &PluginContext) -> Result<()> {
        let mounted = self.factory.mount(ctx.clone()).await?;
        self.mounted = Some(mounted);
        Ok(())
    }

    async fn on_enable(&mut self) -> Result<()> {
        if let Some(lifecycle) = self
            .mounted
            .as_mut()
            .and_then(|mounted| mounted.lifecycle.as_mut())
        {
            lifecycle.on_enable().await?;
        }
        Ok(())
    }

    async fn on_disable(&mut self) -> Result<()> {
        if let Some(lifecycle) = self
            .mounted
            .as_mut()
            .and_then(|mounted| mounted.lifecycle.as_mut())
        {
            lifecycle.on_disable().await?;
        }
        Ok(())
    }

    async fn on_unload(&mut self) -> Result<()> {
        if let Some(mut mounted) = self.mounted.take()
            && let Some(lifecycle) = mounted.lifecycle.as_mut()
        {
            lifecycle.on_unload().await?;
        }
        Ok(())
    }

    async fn on_config_changed(&mut self, config: &Value) -> Result<()> {
        if let Some(lifecycle) = self
            .mounted
            .as_mut()
            .and_then(|mounted| mounted.lifecycle.as_mut())
        {
            lifecycle.on_config_changed(config).await?;
        }
        Ok(())
    }

    async fn on_event(&self, event: &Event) -> Result<PluginResponse> {
        if let Some(lifecycle) = self
            .mounted
            .as_ref()
            .and_then(|mounted| mounted.lifecycle.as_ref())
        {
            return lifecycle.on_event(event).await;
        }
        Ok(PluginResponse::Continue)
    }

    async fn on_interaction(
        &self,
        event: &Event<'_>,
        interaction: &crate::api::interaction::PluginInteraction,
    ) -> Result<PluginResponse> {
        if let Some(lifecycle) = self
            .mounted
            .as_ref()
            .and_then(|mounted| mounted.lifecycle.as_ref())
        {
            return lifecycle.on_interaction(event, interaction).await;
        }
        Ok(PluginResponse::Continue)
    }

    async fn on_message_listen(&self, event: &Event) -> Result<PluginResponse> {
        if let Some(lifecycle) = self
            .mounted
            .as_ref()
            .and_then(|mounted| mounted.lifecycle.as_ref())
        {
            return lifecycle.on_message_listen(event).await;
        }
        Ok(PluginResponse::Continue)
    }

    async fn on_message(
        &self,
        event: &Event,
        session_id: &str,
        callback_id: &str,
    ) -> Result<PluginResponse> {
        if let Some(lifecycle) = self
            .mounted
            .as_ref()
            .and_then(|mounted| mounted.lifecycle.as_ref())
        {
            return lifecycle.on_message(event, session_id, callback_id).await;
        }
        Ok(PluginResponse::Continue)
    }

    async fn status_metrics(
        &self,
        request: &PluginStatusRequest,
    ) -> Result<Vec<PluginStatusMetric>> {
        if let Some(lifecycle) = self
            .mounted
            .as_ref()
            .and_then(|mounted| mounted.lifecycle.as_ref())
        {
            return lifecycle.status_metrics(request).await;
        }
        Ok(Vec::new())
    }

    async fn handle_web_request(&self, req: &PluginWebRequest) -> PluginWebResponse {
        if let Some(router) = self
            .mounted
            .as_ref()
            .and_then(|mounted| mounted.web_router.as_ref())
        {
            return router.handle(req).await;
        }
        PluginWebResponse::NotFound
    }
}
