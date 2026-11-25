//! 插件 Web 页面的不可变快照管理，与插件 Web 路由遵循相同的激活与撤销生命周期边界。
use super::*;
use crate::auth::{Capability, Role};
use crate::plugin::{PluginWebAccessPolicy, PluginWebPage};

pub(super) struct RegisteredPages {
    pub generation: u64,
    pub pages: Vec<Arc<PluginWebPage>>,
}

/// 已启用插件页面的只读快照，携带激活批次号（generation）以便在插件停用或重载时吊销已签发的静态资源会话。
#[derive(Debug, Clone)]
pub struct PluginWebPageView {
    pub plugin: String,
    pub generation: u64,
    pub page: Arc<PluginWebPage>,
}

fn allowed(page: &PluginWebPage, role: Role) -> bool {
    match page.access {
        PluginWebAccessPolicy::Authenticated => true,
        PluginWebAccessPolicy::ManageRuntime => role.has(Capability::ManageRuntime),
        PluginWebAccessPolicy::Public => false,
        _ => false,
    }
}

impl PluginHost {
    /// 列出当前登录角色有权访问的已启用插件页面（仅读取快照，不调用任何插件回调）。
    pub async fn plugin_web_pages(&self, role: Role) -> Vec<PluginWebPageView> {
        let configs = self.plugin_registry.read().await;
        let registry = self.web_pages.read().await;
        let mut pages = Vec::new();
        for (plugin, registered) in registry.iter() {
            if !configs.get(plugin).is_some_and(|config| config.enabled) {
                continue;
            }
            for page in registered.pages.iter().filter(|page| allowed(page, role)) {
                pages.push(PluginWebPageView {
                    plugin: plugin.clone(),
                    generation: registered.generation,
                    page: page.clone(),
                });
            }
        }
        pages.sort_by(|a, b| (&a.plugin, &a.page.id).cmp(&(&b.plugin, &b.page.id)));
        pages
    }

    /// 在签发或校验页面访问授权前，先根据当前插件启用状态与调用者角色查找匹配的页面快照。
    pub async fn plugin_web_page(
        &self,
        plugin: &str,
        id: &str,
        role: Role,
    ) -> Option<PluginWebPageView> {
        if !self
            .get_plugin_config(plugin)
            .await
            .is_some_and(|config| config.enabled)
        {
            return None;
        }
        let registry = self.web_pages.read().await;
        let registered = registry.get(plugin)?;
        let page = registered
            .pages
            .iter()
            .find(|page| page.id == id && allowed(page, role))?;
        Some(PluginWebPageView {
            plugin: plugin.into(),
            generation: registered.generation,
            page: page.clone(),
        })
    }
}
