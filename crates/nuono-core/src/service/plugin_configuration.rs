//! 个人级与群级插件配置读写共用的应用层服务。
use crate::{
    config::{ConfigValue, PluginConfigManager},
    db::Database,
    error::{NuonoError, Result},
};
use nuono_app::configuration::ConfigurationActor;
use nuono_storage::plugin_overrides::{OverridePatch, OverrideTarget};
use std::{collections::HashMap, sync::Arc};

pub async fn resolve(
    db: &Database,
    manager: &PluginConfigManager,
    plugin: &str,
    target: Option<OverrideTarget>,
) -> Result<nuono_plugin::config::RuntimePluginConfiguration> {
    let (policy, saved, row) = db.plugin_configuration_snapshot(plugin, target).await?;
    resolve_snapshot(manager, plugin, target, policy, saved, row).await
}
pub(crate) async fn resolve_snapshot(
    manager: &PluginConfigManager,
    plugin: &str,
    target: Option<OverrideTarget>,
    policy: nuono_storage::entity::plugins::Model,
    saved: nuono_storage::plugins::PluginConfiguration,
    row: Option<nuono_storage::plugin_overrides::Override>,
) -> Result<nuono_plugin::config::RuntimePluginConfiguration> {
    let mut values = manager.runtime_from_configuration(plugin, &saved).await?;
    let permitted = if policy.plugin_role == nuono_app::plugin_role::PluginRole::Service.as_str() {
        false
    } else {
        match target {
            Some(OverrideTarget::User(_)) => policy.allow_user_overrides,
            Some(OverrideTarget::Group(_)) => policy.allow_group_overrides,
            None => false,
        }
    };
    let row = if permitted { row } else { None };
    let enabled = policy.enabled && row.as_ref().and_then(|row| row.enabled).unwrap_or(true);
    if let Some(row) = row {
        let schema = manager
            .get_schema(plugin)
            .await
            .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))?;
        let object = values
            .as_object_mut()
            .ok_or_else(|| NuonoError::ConfigValidation("全局配置不是对象".into()))?;
        for raw in [&row.config_patch_json, &row.service_patch_json] {
            let patch: serde_json::Map<String, serde_json::Value> = serde_json::from_str(raw)?;
            for (key, value) in patch {
                let field = schema.get(&key).ok_or_else(|| {
                    NuonoError::ConfigValidation(format!("覆盖字段 {key} 已不在声明中，需转换"))
                })?;
                let scope_permitted = match target {
                    Some(OverrideTarget::User(_)) => field.user_scoped(),
                    Some(OverrideTarget::Group(_)) => field.group_scoped(),
                    None => false,
                };
                if !scope_permitted {
                    return Err(NuonoError::ConfigValidation(format!(
                        "覆盖字段 {key} 已不允许此对象配置"
                    )));
                }
                let typed: ConfigValue = serde_json::from_value(value.clone())?;
                crate::config::plugin_config::validate_config_value(&key, &typed, field)
                    .map_err(NuonoError::ConfigValidation)?;
                object.insert(key, value);
            }
        }
    }
    Ok(nuono_plugin::config::RuntimePluginConfiguration {
        values: Arc::new(values),
        enabled,
    })
}

pub async fn save(
    db: &Database,
    manager: &PluginConfigManager,
    actor: &ConfigurationActor,
    plugin: &str,
    target: OverrideTarget,
    patch: OverridePatch,
) -> Result<Option<nuono_storage::plugin_overrides::Override>> {
    if db.plugin_policy(plugin).await?.plugin_role
        == nuono_app::plugin_role::PluginRole::Service.as_str()
    {
        return Err(NuonoError::PermissionDenied(
            "底层服务插件不支持个人或群级配置覆盖".into(),
        ));
    }
    let schema = manager
        .get_schema(plugin)
        .await
        .ok_or_else(|| NuonoError::PluginNotFound(plugin.into()))?;
    for key in patch
        .set_config
        .keys()
        .chain(patch.set_service.keys())
        .chain(&patch.remove_config)
        .chain(&patch.remove_service)
    {
        let field = schema
            .get(key)
            .ok_or_else(|| NuonoError::ConfigValidation(format!("未声明的配置字段 {key}")))?;
        let permitted = match target {
            OverrideTarget::User(_) => field.user_scoped(),
            OverrideTarget::Group(_) => field.group_scoped(),
        };
        if !permitted {
            return Err(NuonoError::PermissionDenied(format!(
                "配置字段 {key} 不允许此类对象覆盖"
            )));
        }
        let service = crate::config::plugin_config::is_service_field(key);
        if service != (patch.set_service.contains_key(key) || patch.remove_service.contains(key)) {
            return Err(NuonoError::ConfigValidation(format!(
                "字段 {key} 的业务/服务分类错误"
            )));
        }
    }
    for (key, value) in patch.set_config.iter().chain(&patch.set_service) {
        let typed: ConfigValue = serde_json::from_value(value.clone())?;
        crate::config::plugin_config::validate_config_value(key, &typed, &schema[key])
            .map_err(NuonoError::ConfigValidation)?;
    }
    db.save_plugin_override_with_secrets(
        actor,
        plugin,
        target,
        patch,
        &crate::config::plugin_config::secret_fields(&schema),
    )
    .await
}

pub async fn event_configurations(
    db: &Database,
    manager: &PluginConfigManager,
    event: Option<&nuono_domain::DomainEvent>,
) -> Result<HashMap<String, nuono_plugin::config::RuntimePluginConfiguration>> {
    let target = match event.and_then(|event| event.conversation.as_ref()) {
        Some(nuono_domain::ConversationRef::Direct { participant, .. }) => db
            .account_for_platform_identity(participant)
            .await?
            .map(OverrideTarget::User),
        Some(conversation) => {
            let identity = nuono_storage::groups::GroupIdentity::from_conversation(conversation)?
                .ok_or_else(|| NuonoError::Validation("群对象身份缺失".into()))?;
            let group = db.ensure_group(&identity, "").await?;
            if !group.enabled {
                return Err(NuonoError::PermissionDenied("群对象已停用".into()));
            }
            Some(OverrideTarget::Group(group.id))
        }
        None => None,
    };
    let mut configurations = HashMap::new();
    for plugin in manager.list_configs().await {
        let configuration = match resolve(db, manager, &plugin, target).await {
            Ok(value) => value,
            Err(error) => {
                manager
                    .set_runtime_error(&plugin, Some(error.to_string()))
                    .await;
                nuono_plugin::config::RuntimePluginConfiguration {
                    values: Arc::new(serde_json::json!({})),
                    enabled: false,
                }
            }
        };
        configurations.insert(plugin, configuration);
    }
    Ok(configurations)
}
