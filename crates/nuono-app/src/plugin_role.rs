//! 插件角色分级：区分面向群/用户的业务功能插件与常驻全局的底层支撑服务插件。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 插件角色分级。
///
/// - `Feature`（业务功能插件）：正常展示在群管理与个人配置界面，支持群级开关、群准入校验与配置覆盖委派。
/// - `Service`（底层支撑插件）：对群管理后台与普通用户彻底隐藏，常驻全局加载，仅部署所有者可查看、配置和启停。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRole {
    #[default]
    Feature,
    Service,
}

impl PluginRole {
    /// 返回用于数据库持久化与接口序列化的稳定标识字符串。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Feature => "feature",
            Self::Service => "service",
        }
    }

    /// 从持久化或外部输入字符串解析插件角色。
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "feature" => Some(Self::Feature),
            "service" => Some(Self::Service),
            _ => None,
        }
    }

    /// 是否为底层支撑服务插件。
    #[must_use]
    pub const fn is_service(self) -> bool {
        matches!(self, Self::Service)
    }

    /// 是否为业务功能插件。
    #[must_use]
    pub const fn is_feature(self) -> bool {
        matches!(self, Self::Feature)
    }
}

impl fmt::Display for PluginRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PluginRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("无效的插件角色: {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::PluginRole;

    #[test]
    fn role_roundtrip_and_default() {
        assert_eq!(PluginRole::default(), PluginRole::Feature);
        assert_eq!(PluginRole::Feature.as_str(), "feature");
        assert_eq!(PluginRole::Service.as_str(), "service");
        assert_eq!(PluginRole::parse("feature"), Some(PluginRole::Feature));
        assert_eq!(PluginRole::parse("service"), Some(PluginRole::Service));
        assert_eq!(PluginRole::parse("unknown"), None);
        assert_eq!(
            serde_json::to_string(&PluginRole::Service).expect("序列化 PluginRole 失败"),
            "\"service\""
        );
        assert_eq!(
            serde_json::from_str::<PluginRole>("\"feature\"").expect("反序列化 PluginRole 失败"),
            PluginRole::Feature
        );
    }
}
