//! 当前程序的插件目录。数据库历史行不参与成员判断，运行失败不移除身份。

use super::loading::PluginLoadPlan;
use super::*;

impl PluginHost {
    /// 当前程序已发现的插件，包含禁用和加载失败项。
    ///
    /// `enabled` 表示当前存在可用且启用的实例；持久化启停意图仍由数据库管理。
    /// 目录元数据不授予配置、路由或 secret 权限。返回的 schema 仅来自已发布声明。
    /// 直接注入的实例也参与目录，便于嵌入使用与宿主测试；本方法不创建新实例。
    pub async fn installed_plugin_infos(&self) -> HashMap<String, PluginInfoView> {
        let mut infos = self.installed_plugins.read().await.clone();
        for (name, info) in &mut infos {
            info.display_name = plugin_display_name(name, &info.display_name);
            info.enabled = false;
            info.config_schema = serde_json::json!({});
        }
        let plugins = self.plugins.read().await.clone();
        let mut present = Vec::with_capacity(plugins.len());
        for plugin in plugins.iter() {
            let Ok(name) = Self::call_with_catch(async { Ok(plugin.name().to_string()) }).await
            else {
                continue;
            };
            if !infos.contains_key(&name) {
                let Ok(plugin) = plugin.plugin.try_read() else {
                    infos.insert(
                        name.clone(),
                        PluginInfoView {
                            display_name: name.clone(),
                            ..Default::default()
                        },
                    );
                    present.push(name);
                    continue;
                };
                let info = Self::call_with_catch(async {
                    Ok(PluginInfoView {
                        display_name: plugin_display_name(&name, plugin.display_name()),
                        version: plugin.version().to_string(),
                        description: plugin.description().to_string(),
                        plugin_role: plugin.plugin_role(),
                        required_dependencies: plugin
                            .required_dependencies()
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                        optional_dependencies: plugin
                            .optional_dependencies()
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                        message_listener: plugin.message_listener(),
                        ..PluginInfoView::default()
                    })
                })
                .await
                .unwrap_or_else(|_| PluginInfoView {
                    display_name: name.clone(),
                    description: "插件元数据读取失败".into(),
                    ..PluginInfoView::default()
                });
                infos.insert(name.clone(), info);
            }
            present.push(name);
        }
        let registry = self.plugin_registry.read().await;
        let declarations = self.config_declarations.read().await;
        for name in present {
            if let Some(info) = infos.get_mut(&name) {
                info.enabled = registry.get(&name).is_some_and(|config| config.enabled);
                info.config_schema = declarations
                    .get(&name)
                    .and_then(|fields| crate::config::plugin_fields_to_schema(fields).ok())
                    .and_then(|schema| serde_json::to_value(schema).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
            }
        }
        infos
    }

    /// 仅回答当前程序是否包含该身份，不查询历史数据库记录。
    pub async fn is_plugin_installed(&self, name: &str) -> bool {
        if self.installed_plugins.read().await.contains_key(name) {
            return true;
        }
        self.plugins
            .read()
            .await
            .iter()
            .any(|plugin| plugin.name() == name)
    }

    pub(super) async fn remember_install_plan(&self, plan: &PluginLoadPlan) {
        let mut installed = self.installed_plugins.write().await;
        for rejected in &plan.rejected {
            installed
                .entry(rejected.name.clone())
                .or_insert_with(|| PluginInfoView {
                    display_name: rejected.display_name.clone(),
                    description: "插件声明无效，未加载".into(),
                    ..PluginInfoView::default()
                });
        }
        // 同名有效声明优先，不被先前拒绝的同名候选遮盖。
        for candidate in &plan.candidates {
            installed.insert(
                candidate.declaration.name.clone(),
                declaration_info(&candidate.declaration),
            );
        }
    }

    pub(super) async fn remember_plugin_declaration(&self, declaration: &PluginDeclaration) {
        self.installed_plugins
            .write()
            .await
            .insert(declaration.name.clone(), declaration_info(declaration));
    }

    pub(super) async fn remember_plugin_info(&self, name: &str, mut info: PluginInfoView) {
        info.display_name = plugin_display_name(name, &info.display_name);
        info.enabled = false;
        info.config_schema = serde_json::json!({});
        self.installed_plugins
            .write()
            .await
            .entry(name.to_string())
            .or_insert(info);
    }
}

fn declaration_info(declaration: &PluginDeclaration) -> PluginInfoView {
    PluginInfoView {
        display_name: declaration.display_name.clone(),
        version: declaration.version.clone(),
        description: declaration.description.clone(),
        plugin_role: declaration.plugin_role,
        required_dependencies: declaration.required_dependencies.clone(),
        optional_dependencies: declaration.optional_dependencies.clone(),
        message_listener: declaration.message_listener.clone(),
        ..PluginInfoView::default()
    }
}
