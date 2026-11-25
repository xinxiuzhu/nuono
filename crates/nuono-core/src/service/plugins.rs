//! 当前程序的插件目录，以及独立保留的持久化启停和群管理策略。

use std::collections::HashMap;

use crate::core::plugin_host::{PluginHost, PluginInfoView};
use crate::db::Database;
use crate::error::Result;

// 持久化记录用于策略读写与历史数据保留，不能直接作为当前安装清单。
pub use nuono_storage::plugins::{
    PluginInfo, get_plugins, is_plugin_enabled, set_plugin_enabled, sync_plugin,
};

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct InstalledPluginInfo {
    /// 声明校验失败的新插件可能还没有持久化记录；管理操作以 name 为身份。
    pub id: Option<String>,
    pub name: String,
    /// 源码声明的展示名称；路由与持久化策略始终使用 name。
    pub display_name: String,
    pub version: String,
    #[schema(value_type = String, example = "feature")]
    pub plugin_role: nuono_app::plugin_role::PluginRole,
    pub required_dependencies: Vec<String>,
    pub optional_dependencies: Vec<String>,
    pub enabled: bool,
    pub customer_toggleable: bool,
    pub allow_user_overrides: bool,
}

/// 从本次程序发现的插件出发，只叠加同名持久化策略。
/// 禁用或加载失败仍属于当前目录，历史 DB 行不能反向增加目录成员。
pub async fn get_installed_plugins(
    db: &Database,
    host: &PluginHost,
) -> Result<Vec<InstalledPluginInfo>> {
    let installed = host.installed_plugin_infos().await;
    if installed.is_empty() {
        return Ok(Vec::new());
    }
    Ok(project_installed_plugins(installed, get_plugins(db).await?))
}

fn project_installed_plugins(
    installed: HashMap<String, PluginInfoView>,
    persisted: Vec<PluginInfo>,
) -> Vec<InstalledPluginInfo> {
    let persisted: HashMap<_, _> = persisted
        .into_iter()
        .map(|plugin| (plugin.name.clone(), plugin))
        .collect();
    let mut plugins: Vec<_> = installed
        .into_iter()
        .map(|(name, metadata)| {
            let policy = persisted.get(&name);
            InstalledPluginInfo {
                id: policy.map(|plugin| plugin.id.clone()),
                display_name: crate::core::plugin_host::plugin_display_name(
                    &name,
                    &metadata.display_name,
                ),
                version: metadata.version,
                plugin_role: metadata.plugin_role,
                required_dependencies: metadata.required_dependencies,
                optional_dependencies: metadata.optional_dependencies,
                enabled: policy.is_some_and(|plugin| plugin.enabled),
                customer_toggleable: policy.is_some_and(|plugin| plugin.customer_toggleable),
                allow_user_overrides: policy.is_some_and(|plugin| plugin.allow_user_overrides),
                name,
            }
        })
        .collect();
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    plugins
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persisted(name: &str, enabled: bool) -> PluginInfo {
        PluginInfo {
            id: name.into(),
            name: name.into(),
            plugin_role: nuono_app::plugin_role::PluginRole::Feature,
            enabled,
            customer_toggleable: false,
            allow_user_overrides: false,
        }
    }

    #[test]
    fn installed_directory_excludes_history_and_preserves_current_policy() {
        let current = PluginInfoView {
            display_name: "Friendly Plugin".into(),
            version: "2.0".into(),
            ..Default::default()
        };
        let plugins = project_installed_plugins(
            HashMap::from([("CurrentPlugin".into(), current)]),
            vec![
                persisted("RemovedPlugin", true),
                persisted("CurrentPlugin", false),
            ],
        );
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "CurrentPlugin");
        assert_eq!(plugins[0].display_name, "Friendly Plugin");
        assert_eq!(plugins[0].id.as_deref(), Some("CurrentPlugin"));
        assert_eq!(plugins[0].version, "2.0");
        assert!(!plugins[0].enabled);
        assert!(!plugins[0].customer_toggleable);
    }

    #[test]
    fn declared_plugin_without_database_row_remains_visible_but_inactive() {
        let plugins = project_installed_plugins(
            HashMap::from([("InvalidDeclaration".into(), PluginInfoView::default())]),
            vec![],
        );
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "InvalidDeclaration");
        assert_eq!(plugins[0].display_name, "InvalidDeclaration");
        assert!(plugins[0].id.is_none());
        assert!(!plugins[0].enabled);
    }

    #[test]
    fn empty_program_does_not_inherit_any_historical_plugins() {
        assert!(
            project_installed_plugins(HashMap::new(), vec![persisted("Removed", true)]).is_empty()
        );
    }
}
