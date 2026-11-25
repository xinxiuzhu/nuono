mod catalog;
mod config;
mod lifecycle;
#[cfg(test)]
mod presentation_tests;

use nuono_core::core::permission::PermissionLevel;
use nuono_core::core::plugin_host::{PluginDiagnostics, PluginTriggerPolicy};
use nuono_core::core::{PluginArea, TriggerType};
use nuono_core::service::plugins::InstalledPluginInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;

pub use catalog::{
    __path_detail, __path_icon, __path_list, __path_list_services, __path_list_triggers,
    __path_runtime_overview, detail, icon, list, list_services, list_triggers, runtime_overview,
};
pub use config::{
    __path_config_status, __path_reset_config_fields, __path_save_config, __path_save_service,
    config_status, reset_config_fields, save_config, save_service,
};
#[cfg(test)]
use lifecycle::DiagnosticsBody;
pub use lifecycle::{
    __path_reload, __path_save_diagnostics, __path_save_trigger_policy, __path_toggle,
    __path_trigger_policy, reload, save_diagnostics, save_trigger_policy, toggle, trigger_policy,
};

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::PluginListItem)]
pub struct PluginListItem {
    #[serde(flatten)]
    pub info: InstalledPluginInfo,
    pub description: String,
    /// 构建时嵌入的可选插件图标；缺失时管理界面显示默认图标。
    pub icon_url: Option<String>,
    /// 当前宿主启动以来的实际插件调用统计，以及当前生效的慢调用阈值。
    pub diagnostics: PluginDiagnostics,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::PluginDetail)]
pub struct PluginDetail {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub description: String,
    #[schema(value_type = String, example = "feature")]
    pub plugin_role: nuono_app::plugin_role::PluginRole,
    pub required_dependencies: Vec<String>,
    pub optional_dependencies: Vec<String>,
    pub icon_url: Option<String>,
    /// 实例禁用或重建保留累计值；宿主重启统计归零，阈值从数据库恢复。
    pub diagnostics: PluginDiagnostics,
    pub enabled: bool,
    /// 插件在宿主侧的触发准入与限流策略，独立于插件业务设置和慢调用诊断。
    pub trigger_policy: PluginTriggerPolicy,
    /// 客户能否在自己管理的群里调整这个插件（开关/配置/恢复默认）。
    pub customer_toggleable: bool,
    pub allow_user_overrides: bool,
    pub status: String,
    pub service_config: PluginServiceConfig,
    /// 加载时声明的自动监听范围；没有声明的插件不显示监听开关。
    pub message_listener: Option<MessageListenerInfo>,
    pub sv_list: Vec<ServiceView>,
    pub config_fields: Vec<nuono_core::core::config_schema::PluginConfigField>,
    pub schema: nuono_core::config::ConfigSchema,
    pub values: HashMap<String, nuono_core::config::ConfigValue>,
    /// 已在持久化层显式覆盖（非代码默认）的业务配置键列表。
    pub overridden_keys: Vec<String>,
    pub config_status: nuono_core::config::PluginConfigStatus,
    pub commands: Vec<CommandInfo>,
    pub prefix: Vec<String>,
    pub force_prefix: Vec<String>,
    pub alias: Vec<String>,
    #[schema(value_type = String, example = "Everyone")]
    pub min_permission: PermissionLevel,
    pub area: String,
    pub disable_force_prefix: bool,
    pub allow_empty_prefix: bool,
    pub black_list: Vec<String>,
    pub white_list: Vec<String>,
    pub trigger_summary: TriggerSummary,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::MessageListenerInfo)]
pub struct MessageListenerInfo {
    pub description: String,
    pub text_pattern: Option<String>,
    pub segment_types: Vec<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::CommandInfo)]
pub struct CommandInfo {
    pub name: String,
    pub display_name: Option<String>,
    pub description: String,
    pub aliases: Vec<String>,
    pub usage: String,
    pub permission: String,
    pub priority: i32,
    pub enabled: bool,
    pub plugin_name: Option<String>,
    pub sv_name: Option<String>,
    pub trigger_type: String,
    pub strip_prefix: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = api::plugins::PluginServiceConfig)]
#[serde(default)]
pub struct PluginServiceConfig {
    #[schema(value_type = String, example = "Everyone")]
    pub min_permission: PermissionLevel,
    pub area: String,
    pub black_list: Vec<String>,
    pub white_list: Vec<String>,
    pub prefix: Vec<String>,
    pub force_prefix: Vec<String>,
    pub disable_force_prefix: bool,
    /// 插件自动监听的全局默认值；支持能力由详情的 message_listener 表示。
    pub auto_listen_enabled: bool,
    /// 已停用，仅用于旧客户端兼容；不改变前缀路由，服务设置保存不再写入此字段。
    pub allow_empty_prefix: bool,
    pub alias: Vec<String>,
}

impl Default for PluginServiceConfig {
    fn default() -> Self {
        Self {
            min_permission: PermissionLevel::Everyone,
            area: "ALL".to_string(),
            black_list: Vec::new(),
            white_list: Vec::new(),
            prefix: Vec::new(),
            force_prefix: Vec::new(),
            disable_force_prefix: false,
            auto_listen_enabled: false,
            allow_empty_prefix: false,
            alias: Vec::new(),
        }
    }
}

/// 显式服务设置补丁；未提交的字段保持原值，空列表用于明确清除。
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
#[schema(as = api::plugins::PluginServicePatch)]
pub struct PluginServicePatch {
    #[schema(value_type = Option<String>, example = "Everyone")]
    pub min_permission: Option<PermissionLevel>,
    pub area: Option<String>,
    pub black_list: Option<Vec<String>>,
    pub white_list: Option<Vec<String>>,
    pub prefix: Option<Vec<String>>,
    pub force_prefix: Option<Vec<String>>,
    pub disable_force_prefix: Option<bool>,
    pub auto_listen_enabled: Option<bool>,
    pub alias: Option<Vec<String>>,
}

impl PluginServicePatch {
    fn into_values(self) -> HashMap<String, nuono_core::config::ConfigValue> {
        use nuono_core::config::ConfigValue;
        let mut values = HashMap::new();
        for (key, value) in [
            ("prefix", self.prefix),
            ("force_prefix", self.force_prefix),
            ("alias", self.alias),
            ("black_list", self.black_list),
            ("white_list", self.white_list),
        ] {
            if let Some(value) = value {
                values.insert(key.into(), ConfigValue::ListStr(value));
            }
        }
        if let Some(value) = self.min_permission {
            values.insert(
                "min_permission".into(),
                ConfigValue::String(value.as_str().to_owned()),
            );
        }
        if let Some(value) = self.area {
            values.insert("area".into(), ConfigValue::String(value));
        }
        if let Some(value) = self.disable_force_prefix {
            values.insert("disable_force_prefix".into(), ConfigValue::Bool(value));
        }
        if let Some(value) = self.auto_listen_enabled {
            values.insert("auto_listen_enabled".into(), ConfigValue::Bool(value));
        }
        values
    }
}

/// 聚合的插件运行时策略与诊断视图（`GET /api/plugins/{name}/runtime`）。
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::PluginRuntimePolicyView)]
pub struct PluginRuntimePolicyView {
    pub plugin_id: String,
    #[schema(value_type = String, example = "feature")]
    pub plugin_role: nuono_app::plugin_role::PluginRole,
    pub enabled: bool,
    pub allow_user_overrides: bool,
    pub allow_group_overrides: bool,
    pub revision: String,
    pub config_version: String,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::PluginRuntimeOverview)]
pub struct PluginRuntimeOverview {
    pub plugin_id: String,
    pub service: PluginServiceConfig,
    pub policy: PluginRuntimePolicyView,
    pub trigger_policy: PluginTriggerPolicy,
    pub diagnostics: PluginDiagnostics,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::ServiceView)]
pub struct ServiceView {
    pub name: String,
    pub config: SvConfig,
    pub triggers: Vec<TriggerView>,
    pub command_count: usize,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::SvConfig)]
pub struct SvConfig {
    #[schema(value_type = String, example = "Everyone")]
    pub min_permission: PermissionLevel,
    /// 服务内规则的最小优先级，保留完整有符号值。
    pub priority: i32,
    pub enabled: bool,
    pub area: String,
    pub black_list: Vec<String>,
    pub white_list: Vec<String>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::TriggerView)]
pub struct TriggerView {
    pub trigger_type: String,
    /// 生成此规则的结构化指令主名称；服务范围由外层 ServiceView 给出。
    pub command_name: Option<String>,
    pub pattern: String,
    pub prefix: String,
    pub block: bool,
    pub to_me: bool,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::ServiceListItem)]
pub struct ServiceListItem {
    pub plugin: String,
    pub plugin_display_name: String,
    pub name: String,
    pub enabled: bool,
    #[schema(value_type = String, example = "Everyone")]
    pub min_permission: PermissionLevel,
    /// 服务内规则的最小优先级，保留完整有符号值。
    pub priority: i32,
    pub area: String,
    pub black_list: Vec<String>,
    pub white_list: Vec<String>,
    pub prefix: Vec<String>,
    pub trigger_count: usize,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::TriggerListItem)]
pub struct TriggerListItem {
    pub plugin_name: String,
    pub plugin_display_name: String,
    pub service_name: String,
    pub trigger_type: String,
    pub pattern: String,
    pub prefix: String,
    pub block: bool,
    pub to_me: bool,
    pub priority: i32,
    #[schema(value_type = String, example = "Everyone")]
    pub min_permission: PermissionLevel,
    pub area: String,
}

#[derive(Debug, Default, Clone, Serialize, utoipa::ToSchema)]
#[schema(as = api::plugins::TriggerSummary)]
pub struct TriggerSummary {
    pub total: usize,
    pub command: usize,
    pub prefix: usize,
    pub suffix: usize,
    pub keyword: usize,
    pub fullmatch: usize,
    pub regex: usize,
    pub message: usize,
    pub file: usize,
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(
            crate::api::personal_plugins::list,
            crate::api::personal_plugins::save
        ))
        .routes(routes!(list))
        .routes(routes!(detail))
        .routes(routes!(runtime_overview))
        .routes(routes!(icon))
        .routes(routes!(save_config, reset_config_fields))
        .routes(routes!(config_status))
        .routes(routes!(
            crate::api::plugin_settings::preview_import,
            crate::api::plugin_settings::import
        ))
        .routes(routes!(
            crate::api::plugin_settings::policy,
            crate::api::plugin_settings::save_policy
        ))
        .routes(routes!(
            crate::api::plugin_settings::read_delegation,
            crate::api::plugin_settings::delegation
        ))
        .routes(routes!(save_service))
        .routes(routes!(toggle))
        .routes(routes!(reload))
        .routes(routes!(save_diagnostics))
        .routes(routes!(trigger_policy, save_trigger_policy))
}

async fn plugin_service_config(state: &AppState, name: &str) -> PluginServiceConfig {
    let service = state.manager.plugin_host.get_plugin_config(name).await;
    let values = state
        .plugin_config
        .get_values(name)
        .await
        .unwrap_or_default();

    let mut service_config = service
        .as_ref()
        .map(|p| PluginServiceConfig {
            min_permission: p.min_permission,
            area: area_to_string(p.area),
            black_list: p.black_list.clone(),
            white_list: p.white_list.clone(),
            prefix: p.prefix.clone(),
            force_prefix: p.force_prefix.clone(),
            disable_force_prefix: p.disable_force_prefix,
            auto_listen_enabled: p.auto_listen_enabled.unwrap_or(false),
            allow_empty_prefix: p.allow_empty_prefix,
            alias: p.alias.clone(),
        })
        .unwrap_or_default();
    apply_saved_service_values(
        &mut service_config,
        &values,
        service
            .as_ref()
            .is_some_and(|p| p.auto_listen_enabled.is_some()),
    );
    service_config
}

async fn ensure_installed(state: &AppState, name: &str) -> nuono_core::error::Result<()> {
    if state.manager.plugin_host.is_plugin_installed(name).await {
        Ok(())
    } else {
        Err(nuono_core::NuonoError::PluginNotFound(name.to_string()))
    }
}

pub(crate) async fn ensure_plugin_accessible(
    state: &AppState,
    caller: &crate::middleware::CallerIdentity,
    name: &str,
) -> nuono_core::error::Result<()> {
    ensure_installed(state, name).await?;
    if state
        .manager
        .plugin_host
        .plugin_role(name)
        .await
        .is_service()
        && !caller.is_owner()
    {
        return Err(nuono_core::NuonoError::PermissionDenied(
            "仅部署所有者可查看或管理底层服务插件".into(),
        ));
    }
    Ok(())
}

fn expected_revision(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_matches('"'))
}

fn area_to_string(area: PluginArea) -> String {
    match area {
        PluginArea::Group => "GROUP",
        PluginArea::Direct => "DIRECT",
        PluginArea::All => "ALL",
        PluginArea::FollowPlugin => "FOLLOW_PLUGIN",
        _ => "ALL",
    }
    .to_string()
}

fn trigger_type_to_string(trigger_type: TriggerType) -> String {
    match trigger_type {
        TriggerType::Prefix => "prefix",
        TriggerType::Suffix => "suffix",
        TriggerType::Keyword => "keyword",
        TriggerType::FullMatch => "fullmatch",
        // 管理台关心的是指令触发匹配语义，无需区分底层 handler 是普通 Command 还是 EventCommand。
        TriggerType::Command | TriggerType::EventCommand => "command",
        TriggerType::Regex => "regex",
        TriggerType::Message => "message",
        TriggerType::File => "file",
        _ => "unknown",
    }
    .to_string()
}

fn summarize_triggers(sv_list: &[ServiceView]) -> TriggerSummary {
    let mut summary = TriggerSummary::default();
    for sv in sv_list {
        for trigger in &sv.triggers {
            summary.total += 1;
            match trigger.trigger_type.as_str() {
                "command" => summary.command += 1,
                "prefix" => summary.prefix += 1,
                "suffix" => summary.suffix += 1,
                "keyword" => summary.keyword += 1,
                "fullmatch" => summary.fullmatch += 1,
                "regex" => summary.regex += 1,
                "message" => summary.message += 1,
                "file" => summary.file += 1,
                _ => {}
            }
        }
    }
    summary
}

fn rule_priority(triggers: &[std::sync::Arc<nuono_core::core::Trigger>]) -> i32 {
    triggers
        .iter()
        .map(|trigger| trigger.priority)
        .min()
        .unwrap_or_default()
}

fn apply_saved_service_values(
    service: &mut PluginServiceConfig,
    values: &HashMap<String, nuono_core::config::ConfigValue>,
    auto_listen_supported: bool,
) {
    if let Some(nuono_core::config::ConfigValue::ListStr(v)) = values.get("prefix") {
        service.prefix = v.clone();
    }
    if let Some(nuono_core::config::ConfigValue::ListStr(v)) = values.get("force_prefix") {
        service.force_prefix = v.clone();
    }
    if let Some(nuono_core::config::ConfigValue::ListStr(v)) = values.get("alias") {
        service.alias = v.clone();
    }
    if let Some(nuono_core::config::ConfigValue::ListStr(v)) = values.get("black_list") {
        service.black_list = v.clone();
    }
    if let Some(nuono_core::config::ConfigValue::ListStr(v)) = values.get("white_list") {
        service.white_list = v.clone();
    }
    if let Some(nuono_core::config::ConfigValue::String(v)) = values.get("min_permission")
        && let Some(level) = PermissionLevel::parse(v)
    {
        service.min_permission = level;
    }
    if let Some(nuono_core::config::ConfigValue::String(v)) = values.get("area") {
        service.area = v.clone();
    }
    if let Some(nuono_core::config::ConfigValue::Bool(v)) = values.get("disable_force_prefix") {
        service.disable_force_prefix = *v;
    }
    if let Some(nuono_core::config::ConfigValue::Bool(v)) = values.get("allow_empty_prefix") {
        service.allow_empty_prefix = *v;
    }
    if auto_listen_supported
        && let Some(nuono_core::config::ConfigValue::Bool(v)) = values.get("auto_listen_enabled")
    {
        service.auto_listen_enabled = *v;
    }
}
