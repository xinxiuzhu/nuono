use std::collections::HashMap;
use std::str::FromStr;

use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::ApiJson as Json;
use crate::middleware::CallerIdentity;
use axum::{Extension, extract::State};
use nuono_core::config::{ConfigValue, CoreCommandPolicy, CoreCommandsConfig};
use nuono_core::core::TriggerRegistry;
use nuono_core::core_commands::{CoreCommandPermission, ResolvedCoreCommand, resolve_commands};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
#[schema(as = api::commands::CommandOrigin)]
pub enum CommandOrigin {
    Core,
    #[default]
    Plugin,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::commands::CommandView)]
pub struct CommandView {
    pub origin: CommandOrigin,
    pub name: String,
    pub display_name: Option<String>,
    pub aliases: Vec<String>,
    /// 系统核心指令的内置前缀触发形式（例如 `nuono 帮助`、`nuono help`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin_triggers: Vec<String>,
    /// 所属插件配置的命令前缀（空列表表示无需前缀，用于前端精确冲突分析）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<String>,
    pub description: String,
    pub usage: String,
    pub permission: String,
    /// 系统核心指令不可下调的最低安全权限下限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_permission: Option<String>,
    pub priority: i32,
    pub plugin_name: Option<String>,
    pub sv_name: Option<String>,
    pub trigger_type: String,
    pub enabled: bool,
    pub strip_prefix: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::commands::CoreCommandItemView)]
pub struct CoreCommandItemView {
    pub id: String,
    pub name: String,
    pub english: String,
    pub display_name: String,
    pub description: String,
    pub usage: String,
    pub enabled: bool,
    pub permission: String,
    pub default_permission: String,
    pub min_permission: String,
    pub aliases: Vec<String>,
    pub builtin_triggers: Vec<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::commands::CoreCommandsConfigView)]
pub struct CoreCommandsConfigView {
    pub prefix: String,
    pub commands: Vec<CoreCommandItemView>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::commands::UpdateCoreCommandPolicy)]
pub struct UpdateCoreCommandPolicy {
    pub id: String,
    pub enabled: bool,
    pub permission: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(as = api::commands::UpdateCoreCommandsPayload)]
pub struct UpdateCoreCommandsPayload {
    pub prefix: String,
    pub commands: Vec<UpdateCoreCommandPolicy>,
}

fn core_config_view(prefix: String, resolved: Vec<ResolvedCoreCommand>) -> CoreCommandsConfigView {
    let commands = resolved
        .into_iter()
        .map(|item| CoreCommandItemView {
            id: item.definition.key.into(),
            name: item.definition.name.into(),
            english: item.definition.english.into(),
            display_name: item.definition.display_name.into(),
            description: item.definition.description.into(),
            usage: item.usage,
            enabled: item.enabled,
            permission: item.permission.as_str().into(),
            default_permission: item.definition.default_permission.as_str().into(),
            min_permission: item.definition.min_permission.as_str().into(),
            aliases: item.aliases,
            builtin_triggers: item.builtin_triggers,
        })
        .collect();
    CoreCommandsConfigView { prefix, commands }
}

#[utoipa::path(
    get,
    path = "/",
    operation_id = "commands_list_commands",
    tag = "commands",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<CommandView>>))
)]
pub async fn list_commands(
    State(state): State<AppState>,
) -> crate::api::ApiReply<Vec<CommandView>> {
    let plugin_prefixes: HashMap<String, Vec<String>> = state
        .manager
        .plugin_host
        .list_plugin_configs()
        .await
        .into_iter()
        .map(|cfg| {
            let prefixes = cfg
                .prefix
                .iter()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .collect();
            (cfg.name.clone(), prefixes)
        })
        .collect();

    let registry = &state.manager.event_handler.trigger_registry;
    let mut plugin_commands: Vec<CommandView> = registry
        .get_all_commands()
        .iter()
        .map(|c| {
            let prefixes = c
                .plugin_name
                .as_deref()
                .and_then(|plugin| plugin_prefixes.get(plugin))
                .cloned()
                .unwrap_or_default();
            CommandView {
                origin: CommandOrigin::Plugin,
                name: c.name.clone(),
                display_name: c.display_name.clone(),
                aliases: c.aliases.clone(),
                builtin_triggers: vec![],
                prefixes,
                description: c.description.clone(),
                usage: c.usage.clone(),
                permission: format!("{:?}", c.permission),
                min_permission: None,
                priority: c.priority,
                plugin_name: c.plugin_name.clone(),
                sv_name: c.sv_name.clone(),
                trigger_type: match c.trigger_type {
                    nuono_core::core::TriggerType::Command
                    | nuono_core::core::TriggerType::EventCommand => "Command".to_string(),
                    kind => format!("{kind:?}"),
                },
                enabled: c.enabled,
                strip_prefix: c.strip_prefix,
            }
        })
        .collect();
    plugin_commands.sort_by(|a, b| {
        a.plugin_name
            .cmp(&b.plugin_name)
            .then(a.sv_name.cmp(&b.sv_name))
            .then(a.name.cmp(&b.name))
    });

    let config = state.config.snapshot().await;
    let mut result =
        Vec::with_capacity(nuono_core::core_commands::definitions().len() + plugin_commands.len());
    append_core_commands(&mut result, &config.core_commands);
    result.extend(plugin_commands);
    ApiResponse::ok(result)
}

fn append_core_commands(result: &mut Vec<CommandView>, config: &CoreCommandsConfig) {
    result.extend(
        resolve_commands(config)
            .into_iter()
            .map(|item| CommandView {
                origin: CommandOrigin::Core,
                name: item.definition.key.into(),
                display_name: Some(item.definition.display_name.into()),
                aliases: item.aliases,
                builtin_triggers: item.builtin_triggers,
                prefixes: vec![config.effective_prefix().into()],
                description: item.definition.description.into(),
                usage: item.usage,
                permission: item.permission.as_str().into(),
                min_permission: Some(item.definition.min_permission.as_str().into()),
                priority: 0,
                plugin_name: None,
                sv_name: None,
                trigger_type: "Command".into(),
                enabled: item.enabled,
                strip_prefix: true,
            }),
    );
}

#[utoipa::path(
    get,
    path = "/core/config",
    operation_id = "commands_get_core_config",
    tag = "commands",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<CoreCommandsConfigView>))
)]
pub async fn get_core_config(
    State(state): State<AppState>,
) -> crate::api::ApiReply<CoreCommandsConfigView> {
    let (prefix, resolved) = state.manager.core_commands.resolved().await;
    ApiResponse::ok(core_config_view(prefix, resolved))
}

#[utoipa::path(
    put,
    path = "/core/config",
    operation_id = "commands_update_core_config",
    tag = "commands",
    request_body = UpdateCoreCommandsPayload,
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<CoreCommandsConfigView>))
)]
pub async fn update_core_config(
    State(state): State<AppState>,
    caller: Option<Extension<CallerIdentity>>,
    Json(body): Json<UpdateCoreCommandsPayload>,
) -> crate::api::ApiReply<CoreCommandsConfigView> {
    let mut policies = Vec::with_capacity(body.commands.len());
    for item in body.commands {
        let permission = match CoreCommandPermission::from_str(&item.permission) {
            Ok(value) => value,
            Err(error) => return ApiResponse::from_err(&error),
        };
        policies.push(CoreCommandPolicy {
            id: item.id,
            enabled: item.enabled,
            permission: Some(permission),
            aliases: item.aliases,
        });
    }
    let next = CoreCommandsConfig {
        prefix: body.prefix,
        policies,
    };
    let saved = match state.manager.core_commands.update_config(next).await {
        Ok(saved) => saved,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let actor = caller.map(|caller| caller.0.username);
    let _ = nuono_core::audit::record_event(
        &state.db,
        "core_commands.update",
        None,
        actor.as_deref(),
        serde_json::json!({
            "prefix": saved.effective_prefix(),
            "policies": saved.policies,
            "source": "dashboard",
        }),
    )
    .await;
    let prefix = saved.effective_prefix().to_owned();
    let resolved = resolve_commands(&saved);
    ApiResponse::ok(core_config_view(prefix, resolved))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_commands))
        .routes(routes!(get_core_config, update_core_config))
        .routes(routes!(toggle_command))
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[schema(as = api::commands::TogglePayload)]
pub struct TogglePayload {
    pub enabled: bool,
    #[serde(default)]
    pub origin: CommandOrigin,
    #[serde(default)]
    pub plugin_name: Option<String>,
    #[serde(default)]
    pub sv_name: Option<String>,
}

impl TogglePayload {
    fn target(&self) -> Result<CommandOrigin, &'static str> {
        if self.origin == CommandOrigin::Core
            && (self.plugin_name.is_some() || self.sv_name.is_some())
        {
            return Err("系统核心指令不能指定插件或服务归属");
        }
        Ok(self.origin)
    }
}

#[utoipa::path(
    post,
    path = "/{name}/toggle",
    operation_id = "commands_toggle_command",
    tag = "commands",
    request_body = TogglePayload,
    params(("name" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = crate::api::ApiResponse<super::docs::Empty>))
)]
pub async fn toggle_command(
    State(state): State<AppState>,
    crate::api::input::ApiPath(name): crate::api::input::ApiPath<String>,
    caller: Option<Extension<CallerIdentity>>,
    Json(body): Json<TogglePayload>,
) -> crate::api::ApiReply<()> {
    match body.target() {
        Ok(CommandOrigin::Core) => {
            if let Err(error) = state
                .manager
                .core_commands
                .set_enabled(&name, body.enabled)
                .await
            {
                return ApiResponse::from_err(&error);
            }
            let actor = caller.map(|caller| caller.0.username);
            let _ = nuono_core::audit::record_event(
                &state.db,
                "core_command.toggle",
                None,
                actor.as_deref(),
                serde_json::json!({"command": name, "enabled": body.enabled, "source": "dashboard"}),
            )
            .await;
            return ApiResponse::ok(());
        }
        Ok(CommandOrigin::Plugin) => {}
        Err(error) => return ApiResponse::bad_request(error),
    }
    let registry = &state.manager.event_handler.trigger_registry;

    let changed = if body.plugin_name.is_some() || body.sv_name.is_some() {
        registry.toggle_command_scoped(
            body.plugin_name.as_deref(),
            body.sv_name.as_deref(),
            &name,
            body.enabled,
        )
    } else {
        registry.toggle_command(&name, body.enabled)
    };

    if !changed {
        return crate::api::ApiResponse::not_found("指令不存在");
    }

    if let Some(plugin_name) = body.plugin_name.as_deref() {
        if let Err(e) = state.plugin_config.ensure_service_fields(plugin_name).await {
            return crate::api::ApiResponse::from_err(&e);
        }

        let mut values = state
            .plugin_config
            .get_values(plugin_name)
            .await
            .unwrap_or_default();

        let mut disabled = match values.get("disabled_commands") {
            Some(ConfigValue::ListStr(v)) => v.clone(),
            _ => Vec::new(),
        };

        let command_key = TriggerRegistry::command_config_key(body.sv_name.as_deref(), &name);

        if body.enabled {
            disabled.retain(|item| item != &name && item != &command_key);
        } else if !disabled.iter().any(|item| item == &command_key) {
            disabled.push(command_key);
            disabled.sort();
        }

        values.insert(
            "disabled_commands".to_string(),
            ConfigValue::ListStr(disabled),
        );

        if let Err(e) = state.plugin_config.set_values(plugin_name, values).await {
            return crate::api::ApiResponse::from_err(&e);
        }
    }

    crate::api::ApiResponse::ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_origin_keeps_same_name_core_and_plugin_targets_separate() {
        let plugin: TogglePayload = serde_json::from_value(serde_json::json!({
            "enabled": false, "plugin_name": "Example", "sv_name": "maintenance"
        }))
        .unwrap();
        let core: TogglePayload = serde_json::from_value(serde_json::json!({
            "enabled": false, "origin": "core"
        }))
        .unwrap();
        assert_eq!(plugin.target().unwrap(), CommandOrigin::Plugin);
        assert_eq!(core.target().unwrap(), CommandOrigin::Core);

        let ambiguous: TogglePayload = serde_json::from_value(serde_json::json!({
            "enabled": false, "origin": "core", "plugin_name": "Example"
        }))
        .unwrap();
        assert!(ambiguous.target().is_err());
        assert!(
            serde_json::from_value::<TogglePayload>(serde_json::json!({
                "enabled": false, "origin": "unknown"
            }))
            .is_err()
        );
    }

    #[test]
    fn list_keeps_core_and_plugin_names_and_enabled_states_independent() {
        let key = nuono_core::core_commands::definitions()[0].key;
        let mut commands = vec![CommandView {
            origin: CommandOrigin::Plugin,
            name: key.into(),
            display_name: Some("Plugin title".into()),
            aliases: vec![],
            builtin_triggers: vec![],
            prefixes: vec![],
            description: "plugin command".into(),
            usage: key.into(),
            permission: "Everyone".into(),
            min_permission: None,
            priority: 1,
            plugin_name: Some("Example".into()),
            sv_name: None,
            trigger_type: "Command".into(),
            enabled: true,
            strip_prefix: true,
        }];
        let config = CoreCommandsConfig {
            prefix: "bot".into(),
            policies: vec![CoreCommandPolicy {
                id: key.into(),
                enabled: false,
                permission: Some(CoreCommandPermission::GroupAdmin),
                aliases: vec!["菜单".into()],
            }],
        };
        append_core_commands(&mut commands, &config);
        let matching: Vec<_> = commands
            .iter()
            .filter(|command| command.name == key)
            .collect();
        assert_eq!(matching.len(), 2);
        assert_eq!(matching[0].origin, CommandOrigin::Plugin);
        assert!(matching[0].enabled);
        assert_eq!(matching[0].display_name.as_deref(), Some("Plugin title"));
        assert_eq!(matching[1].origin, CommandOrigin::Core);
        assert!(!matching[1].enabled);
        assert_eq!(matching[1].permission, "GroupAdmin");
        assert_eq!(matching[1].aliases, vec!["菜单"]);
        assert!(matching[1].usage.starts_with("bot "));
        assert!(matching[1].plugin_name.is_none());
    }
}
