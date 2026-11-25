use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use nuono_core::service::plugins::get_installed_plugins;

use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::ApiPath as Path;

use super::{
    CommandInfo, MessageListenerInfo, PluginDetail, PluginListItem, PluginRuntimeOverview,
    PluginRuntimePolicyView, PluginServiceConfig, ServiceListItem, ServiceView, SvConfig,
    TriggerListItem, TriggerView, apply_saved_service_values, area_to_string,
    ensure_plugin_accessible, plugin_service_config, rule_priority, summarize_triggers,
    trigger_type_to_string,
};

#[utoipa::path(
    get,
    path = "/",
    operation_id = "plugins_list",
    tag = "plugins",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<PluginListItem>>))
)]
pub async fn list(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
) -> crate::api::ApiReply<Vec<PluginListItem>> {
    match get_installed_plugins(&state.db, &state.manager.plugin_host).await {
        Ok(plugins) => {
            let metadata = state.manager.plugin_host.installed_plugin_infos().await;
            let mut items = Vec::with_capacity(plugins.len());
            for info in plugins {
                if info.plugin_role.is_service() && !caller.is_owner() {
                    continue;
                }
                let Some(metadata) = metadata.get(&info.name) else {
                    return ApiResponse::not_found(format!("插件不存在: {}", info.name));
                };
                let icon_url = crate::plugin_icons::url(&info.name);
                let diagnostics = state.manager.plugin_host.plugin_diagnostics(&info.name);
                items.push(PluginListItem {
                    info,
                    description: metadata.description.clone(),
                    icon_url,
                    diagnostics,
                });
            }
            ApiResponse::ok(items)
        }
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    get,
    path = "/{name}/icon",
    operation_id = "plugins_icon",
    tag = "plugins",
    params(("name" = String, Path)),
    responses(
        (status = 200, description = "构建时嵌入的插件图标", body = Vec<u8>, content_type = "image/svg+xml"),
        (status = 304, description = "图标未改变"),
        (status = 404, description = "插件没有自定义图标")
    )
)]
pub async fn icon(Path(name): Path<String>, headers: HeaderMap) -> Response {
    match crate::plugin_icons::get(&name) {
        Some(icon) => icon.response(&headers),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/api/services",
    operation_id = "plugins_list_services",
    tag = "plugins",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<ServiceListItem>>))
)]
pub async fn list_services(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
) -> crate::api::ApiReply<Vec<ServiceListItem>> {
    let plugins = match get_installed_plugins(&state.db, &state.manager.plugin_host).await {
        Ok(plugins) => plugins,
        Err(e) => return ApiResponse::from_err(&e),
    };

    let mut items = Vec::new();
    for plugin in plugins {
        if plugin.plugin_role.is_service() && !caller.is_owner() {
            continue;
        }
        let service_config = plugin_service_config(&state, &plugin.name).await;
        let svs = state.manager.plugin_host.get_sv_list(&plugin.name).await;
        for (sv, triggers) in svs {
            let priority = rule_priority(&triggers);

            items.push(ServiceListItem {
                plugin: plugin.name.clone(),
                plugin_display_name: plugin.display_name.clone(),
                name: sv.name.clone(),
                enabled: plugin.enabled,
                min_permission: service_config.min_permission,
                priority,
                area: service_config.area.clone(),
                black_list: service_config.black_list.clone(),
                white_list: service_config.white_list.clone(),
                prefix: service_config.prefix.clone(),
                trigger_count: triggers.len(),
            });
        }
    }

    ApiResponse::ok(items)
}

#[utoipa::path(
    get,
    path = "/api/triggers",
    operation_id = "plugins_list_triggers",
    tag = "plugins",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<TriggerListItem>>))
)]
pub async fn list_triggers(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
) -> crate::api::ApiReply<Vec<TriggerListItem>> {
    let plugins = match get_installed_plugins(&state.db, &state.manager.plugin_host).await {
        Ok(plugins) => plugins,
        Err(e) => return ApiResponse::from_err(&e),
    };

    let mut items = Vec::new();
    for plugin in plugins {
        if plugin.plugin_role.is_service() && !caller.is_owner() {
            continue;
        }
        let service_config = plugin_service_config(&state, &plugin.name).await;
        let svs = state.manager.plugin_host.get_sv_list(&plugin.name).await;
        for (sv, triggers) in svs {
            for trigger in triggers {
                items.push(TriggerListItem {
                    plugin_name: plugin.name.clone(),
                    plugin_display_name: plugin.display_name.clone(),
                    service_name: sv.name.clone(),
                    trigger_type: trigger_type_to_string(trigger.trigger_type),
                    pattern: trigger.pattern.clone(),
                    prefix: trigger.prefix.clone(),
                    block: trigger.block,
                    to_me: trigger.to_me,
                    priority: trigger.priority,
                    min_permission: service_config.min_permission,
                    area: service_config.area.clone(),
                });
            }
        }
    }

    ApiResponse::ok(items)
}

#[utoipa::path(
    get,
    path = "/{name}",
    operation_id = "plugins_detail",
    tag = "plugins",
    params(("name" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<PluginDetail>))
)]
pub async fn detail(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(name): crate::api::input::ApiPath<String>,
) -> crate::api::ApiReply<PluginDetail> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    let plugins = match get_installed_plugins(&state.db, &state.manager.plugin_host).await {
        Ok(plugins) => plugins,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let Some(settings) = plugins.into_iter().find(|plugin| plugin.name == name) else {
        return ApiResponse::not_found(format!("插件不存在: {name}"));
    };
    let metadata = state.manager.plugin_host.installed_plugin_infos().await;
    let Some(info) = metadata.get(&name) else {
        return ApiResponse::not_found(format!("插件不存在: {name}"));
    };
    let enabled = settings.enabled;
    let service = state.manager.plugin_host.get_plugin_config(&name).await;
    let fields = state
        .manager
        .plugin_host
        .get_plugin_config_fields(&name)
        .await;

    let snapshot = match state.plugin_config.management_snapshot(&name).await {
        Ok(snapshot) => snapshot,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let schema = snapshot.schema;
    let mut values = snapshot.values;

    // 表单只携带可编辑的声明字段；避免旧客户端将保留的未知键原样提交。
    values.retain(|key, _| schema.contains_key(key));
    for key in nuono_core::config::plugin_config::secret_fields(&schema) {
        values.remove(&key);
    }

    let all_commands = state
        .manager
        .plugin_host
        .trigger_registry()
        .get_all_commands();
    let commands: Vec<CommandInfo> = all_commands
        .iter()
        .filter(|c| c.plugin_name.as_deref() == Some(&name))
        .map(|c| CommandInfo {
            name: c.name.clone(),
            display_name: c.display_name.clone(),
            description: c.description.clone(),
            aliases: c.aliases.clone(),
            usage: c.usage.clone(),
            permission: format!("{:?}", c.permission),
            priority: c.priority,
            enabled: c.enabled,
            plugin_name: c.plugin_name.clone(),
            sv_name: c.sv_name.clone(),
            trigger_type: format!("{:?}", c.trigger_type),
            strip_prefix: c.strip_prefix,
        })
        .collect();

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

    let svs = state.manager.plugin_host.get_sv_list(&name).await;
    let sv_list: Vec<ServiceView> = svs
        .into_iter()
        .map(|(sv, triggers)| {
            let priority = rule_priority(&triggers);
            let trigger_views: Vec<TriggerView> = triggers
                .iter()
                .map(|t| TriggerView {
                    trigger_type: trigger_type_to_string(t.trigger_type),
                    command_name: t
                        .origin_command
                        .as_ref()
                        .filter(|origin| {
                            origin.plugin_name == t.plugin_name && origin.sv_name == t.sv_name
                        })
                        .map(|origin| origin.command_name.clone()),
                    pattern: t.pattern.clone(),
                    prefix: t.prefix.clone(),
                    block: t.block,
                    to_me: t.to_me,
                    priority: t.priority,
                })
                .collect();
            ServiceView {
                name: sv.name.clone(),
                config: SvConfig {
                    min_permission: service_config.min_permission,
                    priority,
                    enabled: true,
                    area: service_config.area.clone(),
                    black_list: service_config.black_list.clone(),
                    white_list: service_config.white_list.clone(),
                },
                command_count: trigger_views.len(),
                triggers: trigger_views,
            }
        })
        .collect();

    let trigger_summary = summarize_triggers(&sv_list);

    // 从插件配置读取 prefix，缺省时回落到 service config。
    let prefix = match values.get("prefix") {
        Some(nuono_core::config::ConfigValue::ListStr(v)) => v.clone(),
        _ => service_config.prefix.clone(),
    };

    ApiResponse::ok(PluginDetail {
        name: name.clone(),
        display_name: info.display_name.clone(),
        version: info.version.clone(),
        description: info.description.clone(),
        plugin_role: info.plugin_role,
        required_dependencies: info.required_dependencies.clone(),
        optional_dependencies: info.optional_dependencies.clone(),
        icon_url: crate::plugin_icons::url(&name),
        diagnostics: state.manager.plugin_host.plugin_diagnostics(&name),
        trigger_policy: state.manager.plugin_host.plugin_trigger_policy(&name),
        enabled,
        customer_toggleable: settings.customer_toggleable,
        allow_user_overrides: settings.allow_user_overrides,
        status: if enabled {
            "active".to_string()
        } else {
            "inactive".to_string()
        },
        service_config: service_config.clone(),
        message_listener: info
            .message_listener
            .as_ref()
            .map(|listener| MessageListenerInfo {
                description: listener.description().to_string(),
                text_pattern: listener.text_pattern().map(str::to_string),
                segment_types: listener.segment_types().to_vec(),
            }),
        sv_list,
        config_fields: fields,
        schema,
        values,
        overridden_keys: snapshot.overridden_keys,
        config_status: snapshot.status,
        commands,
        prefix,
        force_prefix: service_config.force_prefix.clone(),
        alias: service_config.alias.clone(),
        min_permission: service_config.min_permission,
        area: service_config.area.clone(),
        disable_force_prefix: service_config.disable_force_prefix,
        allow_empty_prefix: service_config.allow_empty_prefix,
        black_list: service_config.black_list.clone(),
        white_list: service_config.white_list.clone(),
        trigger_summary,
    })
}

/// 聚合读取单个插件的运行时服务配置、全局覆盖策略、触发准入策略与诊断统计。
#[utoipa::path(
    get,
    path = "/{name}/runtime",
    operation_id = "plugins_runtime_overview",
    tag = "plugins",
    params(("name" = String, Path)),
    responses((status = 200, description = "插件运行时服务配置、策略、准入与诊断聚合视图", body = ApiResponse<PluginRuntimeOverview>))
)]
pub async fn runtime_overview(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(name): crate::api::input::ApiPath<String>,
) -> crate::api::ApiReply<PluginRuntimeOverview> {
    if let Err(error) = ensure_plugin_accessible(&state, &caller, &name).await {
        return ApiResponse::from_err(&error);
    }
    let policy_row = match state.db.plugin_policy(&name).await {
        Ok(row) => row,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let service = plugin_service_config(&state, &name).await;
    let trigger_policy = state.manager.plugin_host.plugin_trigger_policy(&name);
    let diagnostics = state.manager.plugin_host.plugin_diagnostics(&name);
    ApiResponse::ok(PluginRuntimeOverview {
        plugin_id: name,
        service,
        policy: PluginRuntimePolicyView {
            plugin_id: policy_row.plugin_id,
            plugin_role: nuono_app::plugin_role::PluginRole::parse(&policy_row.plugin_role)
                .unwrap_or_default(),
            enabled: policy_row.enabled,
            allow_user_overrides: policy_row.allow_user_overrides,
            allow_group_overrides: policy_row.allow_group_overrides,
            revision: policy_row.revision.to_string(),
            config_version: policy_row.config_version.to_string(),
        },
        trigger_policy,
        diagnostics,
    })
}
