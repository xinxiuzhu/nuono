//! 规范化群级插件配置服务；框架级临时授权令牌与已验证的平台管理员角色彼此独立判定。
use crate::{
    config::PluginConfigManager,
    core::plugin_host::PluginHost,
    db::Database,
    error::{NuonoError, Result},
};
use nuono_app::configuration::{ConfigurationActor, ConfigurationMode};
use nuono_storage::plugin_overrides::{OverridePatch, OverrideTarget};
use serde_json::Value;

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ResolvedPluginConfig {
    pub plugin_name: String,
    pub enabled: bool,
    pub global_enabled: bool,
    pub values: Value,
}
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct GroupPluginFieldState {
    pub key: String,
    pub title: String,
    pub desc: String,
    pub group_scoped: bool,
    pub service: bool,
    pub schema: crate::config::ConfigField,
    pub value: Value,
    pub overridden: bool,
}
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct GroupPluginState {
    pub plugin_name: String,
    pub display_name: String,
    pub enabled: bool,
    pub global_enabled: bool,
    pub enabled_overridden: bool,
    pub customer_toggleable: bool,
    pub fields: Vec<GroupPluginFieldState>,
    pub override_id: Option<String>,
    pub revision: String,
    pub config_version: String,
}

pub async fn ensure_group_settings_writable(
    db: &Database,
    group_id: i64,
    plugin: Option<&str>,
    actor: &ConfigurationActor,
) -> Result<()> {
    let user = db
        .find_user_by_id(actor.user_id)
        .await?
        .filter(|user| user.status == "active")
        .ok_or_else(|| NuonoError::PermissionDenied("账号不可用".into()))?;
    if db.group_by_id(group_id).await?.is_none() {
        return Err(NuonoError::NotFound("群对象不存在".into()));
    }
    match &actor.mode {
        ConfigurationMode::Administrator if matches!(user.role.as_str(), "owner" | "admin") => {
            return Ok(());
        }
        ConfigurationMode::VerifiedPlatformRole(proof) => {
            proof.validate_current()?;
            if proof.group_id() == group_id
                && db
                    .platform_identities(actor.user_id)
                    .await?
                    .iter()
                    .any(|identity| {
                        identity.id == proof.identity_id().to_string()
                            && identity.revision == proof.identity_revision().to_string()
                    })
            {
                if !nuono_app::entitlement::is_group_active(group_id).await {
                    return Err(NuonoError::PermissionDenied("当前群组服务未激活".into()));
                }
                return Ok(());
            }
        }
        _ => {}
    }
    if !db
        .has_group_configuration_grant(actor.user_id, group_id, plugin)
        .await?
    {
        return Err(NuonoError::PermissionDenied("无权配置该群插件".into()));
    }
    if !nuono_app::entitlement::is_group_active(group_id).await {
        return Err(NuonoError::PermissionDenied("当前群组服务未激活".into()));
    }
    Ok(())
}

pub async fn resolve_for_group(
    db: &Database,
    manager: &PluginConfigManager,
    plugin: &str,
    group_id: &str,
) -> Result<ResolvedPluginConfig> {
    let id = group_id
        .parse::<i64>()
        .map_err(|_| NuonoError::Validation("需要 Nuono 群对象 ID".into()))?;
    let resolved =
        super::plugin_configuration::resolve(db, manager, plugin, Some(OverrideTarget::Group(id)))
            .await?;
    Ok(ResolvedPluginConfig {
        plugin_name: plugin.into(),
        enabled: resolved.enabled,
        global_enabled: db.is_plugin_enabled(plugin).await?,
        values: resolved.values.as_ref().clone(),
    })
}
pub async fn is_enabled_for_group(
    db: &Database,
    manager: &PluginConfigManager,
    plugin: &str,
    group_id: &str,
) -> Result<bool> {
    Ok(resolve_for_group(db, manager, plugin, group_id)
        .await?
        .enabled)
}

pub async fn list_for_group(
    db: &Database,
    host: &PluginHost,
    manager: &PluginConfigManager,
    group_id: i64,
    actor: &ConfigurationActor,
) -> Result<Vec<GroupPluginState>> {
    ensure_group_settings_writable(db, group_id, None, actor).await?;
    let mut out = Vec::new();
    for plugin in super::plugins::get_installed_plugins(db, host).await? {
        if plugin.plugin_role.is_service() {
            continue;
        }
        if ensure_group_settings_writable(db, group_id, Some(&plugin.name), actor)
            .await
            .is_err()
        {
            continue;
        }
        let row = db
            .plugin_override(&plugin.name, OverrideTarget::Group(group_id))
            .await?;
        let config = resolve_for_group(db, manager, &plugin.name, &group_id.to_string()).await?;
        let saved = db.plugin_configuration(&plugin.name).await?;
        let mut explicit = serde_json::Map::new();
        if let Some(row) = &row {
            explicit.extend(serde_json::from_str::<serde_json::Map<String, Value>>(
                &row.config_patch_json,
            )?);
            explicit.extend(serde_json::from_str::<serde_json::Map<String, Value>>(
                &row.service_patch_json,
            )?);
        }
        let mut fields = Vec::new();
        for (key, field) in manager.get_schema(&plugin.name).await.unwrap_or_default() {
            if !field.group_scoped() {
                continue;
            }
            fields.push(GroupPluginFieldState {
                service: crate::config::plugin_config::is_service_field(&key),
                schema: field.clone(),
                value: if crate::config::plugin_config::secret_fields(
                    &manager.get_schema(&plugin.name).await.unwrap_or_default(),
                )
                .contains(&key)
                {
                    Value::Null
                } else {
                    config.values.get(&key).cloned().unwrap_or(Value::Null)
                },
                overridden: explicit.contains_key(&key),
                key,
                title: field.title().into(),
                desc: field.desc().into(),
                group_scoped: true,
            });
        }
        fields.sort_by(|a, b| a.key.cmp(&b.key));
        out.push(GroupPluginState {
            plugin_name: plugin.name,
            display_name: plugin.display_name,
            enabled: config.enabled,
            global_enabled: config.global_enabled,
            enabled_overridden: row.as_ref().is_some_and(|row| row.enabled.is_some()),
            customer_toggleable: plugin.customer_toggleable,
            fields,
            override_id: row.as_ref().map(|row| row.id.to_string()),
            revision: row
                .as_ref()
                .map_or_else(|| "0".into(), |row| row.revision.to_string()),
            config_version: saved.config_version.to_string(),
        });
    }
    Ok(out)
}

pub async fn save_override(
    db: &Database,
    host: &PluginHost,
    manager: &PluginConfigManager,
    group_id: i64,
    plugin: &str,
    actor: &ConfigurationActor,
    patch: OverridePatch,
) -> Result<Option<nuono_storage::plugin_overrides::Override>> {
    if !host.is_plugin_installed(plugin).await {
        return Err(NuonoError::PluginNotFound(plugin.into()));
    }
    if host.plugin_role(plugin).await.is_service() {
        return Err(NuonoError::PermissionDenied(
            "底层服务插件不支持群级配置".into(),
        ));
    }
    ensure_group_settings_writable(db, group_id, Some(plugin), actor).await?;
    super::plugin_configuration::save(
        db,
        manager,
        actor,
        plugin,
        OverrideTarget::Group(group_id),
        patch,
    )
    .await
}
