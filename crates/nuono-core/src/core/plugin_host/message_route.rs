//! 插件前缀是消息命名空间；规则匹配和普通事件回调共享这个入口。

use super::{PluginHost, Plugins};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginMessageRoute {
    Unprefixed,
    /// 框架已有的 `/ai ` 显式入口，优先于插件命名空间。
    FrameworkAgent,
    Plugin {
        name: String,
        prefix: String,
    },
    Conflict {
        prefix: String,
        plugins: Vec<String>,
    },
}

impl PluginMessageRoute {
    pub fn allows_plugin(&self, name: &str) -> bool {
        match self {
            Self::Unprefixed => true,
            Self::Plugin { name: owner, .. } => owner == name,
            Self::FrameworkAgent | Self::Conflict { .. } => false,
        }
    }

    pub fn is_explicit(&self) -> bool {
        !matches!(self, Self::Unprefixed)
    }

    pub fn prefix(&self) -> &str {
        match self {
            Self::Plugin { prefix, .. } => prefix,
            Self::Unprefixed | Self::FrameworkAgent | Self::Conflict { .. } => "",
        }
    }

    pub(crate) fn conflict_message(&self) -> Option<String> {
        match self {
            Self::Conflict { prefix, plugins } => Some(format!(
                "插件前缀“{prefix}”冲突（{}），请管理员为这些插件设置不同前缀后重试。",
                plugins.join("、")
            )),
            _ => None,
        }
    }
}

impl PluginHost {
    /// 按最长非空前缀认领消息。停用插件仍保留前缀，避免命令落入其他插件。
    /// 同一最长前缀属于多个插件时拒绝分发，不以加载顺序决定命令的归属。
    pub async fn message_route(&self, text: &str) -> PluginMessageRoute {
        let registry = self.list_plugin_configs().await;
        resolve_message_route(text, registry.iter().map(AsRef::as_ref))
    }
}

fn resolve_message_route<'a>(
    text: &str,
    plugins: impl IntoIterator<Item = &'a Plugins>,
) -> PluginMessageRoute {
    // 与 AgentRuntime::clean_trigger_content 的现有入口一致；不扩展大小写或分隔符。
    if text.starts_with("/ai ") {
        return PluginMessageRoute::FrameworkAgent;
    }
    let text = text.trim();
    let mut longest = 0;
    let mut owners = Vec::new();
    for plugin in plugins {
        for prefix in plugin.effective_prefixes() {
            if prefix.is_empty()
                || prefix.len() < longest
                || !text
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
            {
                continue;
            }
            if prefix.len() > longest {
                longest = prefix.len();
                owners.clear();
            }
            owners.push(plugin.name.clone());
        }
    }
    owners.sort_unstable();
    owners.dedup();
    let prefix = text[..longest].to_owned();
    match owners.len() {
        0 => PluginMessageRoute::Unprefixed,
        1 => PluginMessageRoute::Plugin {
            name: owners.pop().unwrap(),
            prefix,
        },
        _ => PluginMessageRoute::Conflict {
            prefix,
            plugins: owners,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framework_agent_command_precedes_plugin_prefixes_without_expanding_its_syntax() {
        let slash = Plugins::new("Slash").with_prefix(vec!["/".into()]);
        let ai = Plugins::new("AiPrefix").with_prefix(vec!["/ai".into()]);
        let route = resolve_message_route("/ai 问题", [&slash, &ai]);
        assert_eq!(route, PluginMessageRoute::FrameworkAgent);
        assert!(route.is_explicit());
        assert!(!route.allows_plugin("Slash"));
        assert!(!route.allows_plugin("AiPrefix"));
        assert!(!route.allows_plugin("Unprefixed"));
        assert_eq!(route.prefix(), "");
        for text in ["/AI 问题", "/ai\t问题", "/ai", " /ai 问题"] {
            assert!(!matches!(
                resolve_message_route(text, [&slash, &ai]),
                PluginMessageRoute::FrameworkAgent
            ));
        }
    }

    #[test]
    fn longest_explicit_prefix_routes_case_insensitively_and_excludes_empty_plugins() {
        let url = Plugins::new("Links").with_prefix(vec!["url".into()]);
        let music = Plugins::new("Music").with_prefix(vec!["urlmusic".into()]);
        let ambient = Plugins::new("Ambient");
        let configs = [&url, &music, &ambient];
        assert_eq!(
            resolve_message_route("  URLmusic 登录 ", configs),
            PluginMessageRoute::Plugin {
                name: "Music".into(),
                prefix: "URLmusic".into()
            }
        );
        let route = resolve_message_route("url未声明的命令", configs);
        assert!(route.allows_plugin("Links"));
        assert!(!route.allows_plugin("Ambient"));
        assert!(!route.allows_plugin("Music"));
        assert!(route.is_explicit());
        assert_eq!(
            resolve_message_route("普通消息", configs),
            PluginMessageRoute::Unprefixed
        );
    }

    #[test]
    fn duplicate_prefixes_in_one_plugin_are_not_cross_plugin_conflicts() {
        let first = Plugins::new("A")
            .with_prefix(vec!["ww".into(), "WW".into()])
            .with_force_prefix(vec!["ww".into()]);
        let second = Plugins::new("B").with_prefix(vec!["WW".into()]);
        assert_eq!(
            resolve_message_route("ww帮助", [&first]),
            PluginMessageRoute::Plugin {
                name: "A".into(),
                prefix: "ww".into()
            }
        );
        let conflict = resolve_message_route("ww帮助", [&second, &first]);
        assert_eq!(
            conflict,
            PluginMessageRoute::Conflict {
                prefix: "ww".into(),
                plugins: vec!["A".into(), "B".into()]
            }
        );
        assert!(!conflict.allows_plugin("A"));
        assert!(conflict.conflict_message().unwrap().contains("A、B"));
    }

    #[test]
    fn disabled_namespaces_are_reserved_and_unicode_boundaries_are_safe() {
        let mut plugin = Plugins::new("Disabled").with_prefix(vec!["dm".into(), "示例".into()]);
        plugin.enabled = false;
        let other = Plugins::new("Other").with_prefix(vec!["d".into(), "x".into()]);
        assert!(matches!(
            resolve_message_route("dm帮助", [&plugin, &other]),
            PluginMessageRoute::Plugin { name, .. } if name == "Disabled"
        ));
        assert!(matches!(
            resolve_message_route("示例帮助", [&plugin, &other]),
            PluginMessageRoute::Plugin { name, .. } if name == "Disabled"
        ));
        assert_eq!(
            resolve_message_route("你好", [&plugin, &other]),
            PluginMessageRoute::Unprefixed
        );
    }
}
