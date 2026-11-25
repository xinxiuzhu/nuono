//! 统一聊天侧权限评估模型。
//!
//! - [`PermissionLevel`] 为全项目唯一的 5 档正序权限等级（`Everyone < GroupAdmin < GroupOwner < BotAdmin < BotOwner`）。
//! - [`ActorPermission`] 将封禁态（`Banned`）与正常权限等级解耦，避免破坏全序关系。
//! - 聊天端管理权限采用单轨模型：站点账号角色（`Role::Owner` / `Role::Admin`）通过已验证的平台身份绑定自动继承为
//!   `BotOwner` / `BotAdmin`，停用或降级账号会即时收回对应聊天权限。

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::Role;

/// 全项目统一的 5 档聊天权限等级（正序递增：`actor >= required` 即放行）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum PermissionLevel {
    /// 任何人（普通成员）
    #[default]
    Everyone,
    /// 群管理员（含群主、站点管理员、站长）
    GroupAdmin,
    /// 群主（含站点管理员、站长）
    GroupOwner,
    /// 站点管理员（绑定了 `Role::Admin` / `Role::Owner` 账号）
    BotAdmin,
    /// 站长 / 所有者（绑定了 `Role::Owner` 账号）
    BotOwner,
}

impl PermissionLevel {
    /// 稳定的规范文本标识（与 Serde 序列化名称一致）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Everyone => "Everyone",
            Self::GroupAdmin => "GroupAdmin",
            Self::GroupOwner => "GroupOwner",
            Self::BotAdmin => "BotAdmin",
            Self::BotOwner => "BotOwner",
        }
    }

    /// 解析 snake_case 或 PascalCase 权限等级字符串。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "everyone" | "Everyone" => Some(Self::Everyone),
            "group_admin" | "GroupAdmin" => Some(Self::GroupAdmin),
            "group_owner" | "GroupOwner" => Some(Self::GroupOwner),
            "bot_admin" | "BotAdmin" => Some(Self::BotAdmin),
            "bot_owner" | "BotOwner" => Some(Self::BotOwner),
            _ => None,
        }
    }

    /// 判断当前等级是否满足所需门槛。
    pub fn has_permission(self, required: Self) -> bool {
        self >= required
    }
}

impl fmt::Display for PermissionLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 聊天参与者的最终权限裁决结果：封禁态与正常权限等级分离。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActorPermission {
    /// 已被封禁：拒绝一切交互，优先级高于站长与已绑定账号。
    Banned,
    /// 允许交互，持有指定权限等级。
    Allowed(PermissionLevel),
}

impl Default for ActorPermission {
    fn default() -> Self {
        Self::Allowed(PermissionLevel::Everyone)
    }
}

impl ActorPermission {
    /// 构造普通成员（`Everyone`）允许态。
    pub const fn everyone() -> Self {
        Self::Allowed(PermissionLevel::Everyone)
    }

    pub const fn is_banned(self) -> bool {
        matches!(self, Self::Banned)
    }

    pub const fn level(self) -> Option<PermissionLevel> {
        match self {
            Self::Banned => None,
            Self::Allowed(level) => Some(level),
        }
    }

    /// 是否未被封禁且权限满足 `required`。
    pub fn has_permission(self, required: PermissionLevel) -> bool {
        match self {
            Self::Banned => false,
            Self::Allowed(level) => level.has_permission(required),
        }
    }
}

impl From<PermissionLevel> for ActorPermission {
    fn from(level: PermissionLevel) -> Self {
        Self::Allowed(level)
    }
}

/// 聊天侧封禁名单配置。
#[derive(Debug, Clone, Default)]
pub struct PermissionSettings {
    pub banned_users: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    #[error("{0}")]
    Store(String),
    #[error("{0}")]
    Configuration(String),
}

#[async_trait::async_trait]
pub trait PermissionStore: Send + Sync {
    async fn permission_settings(&self) -> Result<PermissionSettings, String>;

    /// 查询已验证平台身份所绑定的有效框架账号角色（账号停用、未验证或命名空间不匹配时返回 `None`）。
    async fn bound_account_role(
        &self,
        _platform: Option<&str>,
        _namespace: Option<&str>,
        _user_id: &str,
    ) -> Result<Option<Role>, String> {
        Ok(None)
    }
}

pub struct PermissionService {
    store: Arc<dyn PermissionStore>,
}

impl PermissionService {
    pub fn new(store: Arc<dyn PermissionStore>) -> Self {
        Self { store }
    }

    /// 评估聊天参与者的最终权限。
    ///
    /// 优先级：封禁名单 > max(绑定账号角色, 平台群角色)。
    pub async fn evaluate(
        &self,
        platform: Option<&str>,
        namespace: Option<&str>,
        user_id: &str,
        group_role: Option<&str>,
    ) -> Result<ActorPermission, PermissionError> {
        let settings = self
            .store
            .permission_settings()
            .await
            .map_err(PermissionError::Store)?;
        let banned = config_id_list(settings.banned_users, "banned_users")?;

        if list_contains_actor(banned.as_deref(), platform, user_id) {
            return Ok(ActorPermission::Banned);
        }

        let bound_role = self
            .store
            .bound_account_role(platform, namespace, user_id)
            .await
            .map_err(PermissionError::Store)?;

        Ok(ActorPermission::Allowed(resolve_chat_level(
            group_role, bound_role,
        )))
    }
}

fn config_id_list(value: Option<Value>, key: &str) -> Result<Option<Vec<Value>>, PermissionError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::Array(values) = value else {
        return Err(PermissionError::Configuration(format!(
            "permission config {key} must be an array"
        )));
    };
    if values.iter().any(|value| !value.is_string()) {
        return Err(PermissionError::Configuration(format!(
            "permission config {key} must contain only string user IDs"
        )));
    }
    Ok(Some(values))
}

pub fn id_matches(entry: &str, platform: Option<&str>, user_id: &str) -> bool {
    if let Some((entry_platform, entry_user)) = entry.split_once(':') {
        platform.is_some_and(|p| p.eq_ignore_ascii_case(entry_platform)) && entry_user == user_id
    } else {
        entry == user_id
    }
}

fn list_contains_actor(list: Option<&[Value]>, platform: Option<&str>, user_id: &str) -> bool {
    list.is_some_and(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .any(|entry| id_matches(entry, platform, user_id))
    })
}

/// 纯函数：融合已绑定站点账号角色与平台群角色，取最高等级。
pub fn resolve_chat_level(group_role: Option<&str>, bound_role: Option<Role>) -> PermissionLevel {
    let from_account = match bound_role {
        Some(Role::Owner) => PermissionLevel::BotOwner,
        Some(Role::Admin) => PermissionLevel::BotAdmin,
        Some(Role::User) | None => PermissionLevel::Everyone,
    };

    let from_group = match group_role {
        Some("owner") => PermissionLevel::GroupOwner,
        Some("admin") => PermissionLevel::GroupAdmin,
        _ => PermissionLevel::Everyone,
    };

    from_account.max(from_group)
}

#[cfg(test)]
mod port_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn monotonic_order_matches_business_hierarchy() {
        assert!(PermissionLevel::Everyone < PermissionLevel::GroupAdmin);
        assert!(PermissionLevel::GroupAdmin < PermissionLevel::GroupOwner);
        assert!(PermissionLevel::GroupOwner < PermissionLevel::BotAdmin);
        assert!(PermissionLevel::BotAdmin < PermissionLevel::BotOwner);
        assert!(PermissionLevel::GroupOwner.has_permission(PermissionLevel::GroupAdmin));
        assert!(!PermissionLevel::GroupAdmin.has_permission(PermissionLevel::GroupOwner));
    }

    #[test]
    fn bound_account_role_automatically_grants_chat_authority() {
        assert_eq!(
            resolve_chat_level(None, Some(Role::Owner)),
            PermissionLevel::BotOwner
        );
        assert_eq!(
            resolve_chat_level(None, Some(Role::Admin)),
            PermissionLevel::BotAdmin
        );
        assert_eq!(
            resolve_chat_level(Some("owner"), Some(Role::User)),
            PermissionLevel::GroupOwner
        );
        assert_eq!(
            resolve_chat_level(Some("admin"), None),
            PermissionLevel::GroupAdmin
        );
        assert_eq!(resolve_chat_level(None, None), PermissionLevel::Everyone);
    }

    #[test]
    fn invalid_permission_config_is_an_error() {
        assert!(config_id_list(Some(json!("12345")), "banned_users").is_err());
        assert!(config_id_list(Some(json!([12345])), "banned_users").is_err());
    }

    struct Store {
        settings: Result<PermissionSettings, String>,
        bound_role: Result<Option<Role>, String>,
    }

    #[async_trait::async_trait]
    impl PermissionStore for Store {
        async fn permission_settings(&self) -> Result<PermissionSettings, String> {
            self.settings.clone()
        }

        async fn bound_account_role(
            &self,
            _platform: Option<&str>,
            _namespace: Option<&str>,
            _user_id: &str,
        ) -> Result<Option<Role>, String> {
            self.bound_role.clone()
        }
    }

    #[tokio::test]
    async fn banned_actor_wins_over_bound_account_and_errors_fail_closed() {
        let service = PermissionService::new(Arc::new(Store {
            settings: Ok(PermissionSettings {
                banned_users: Some(json!(["native-user"])),
            }),
            bound_role: Ok(Some(Role::Owner)),
        }));
        assert_eq!(
            service
                .evaluate(Some("onebot"), Some(""), "native-user", Some("owner"))
                .await
                .unwrap(),
            ActorPermission::Banned
        );

        let failing = PermissionService::new(Arc::new(Store {
            settings: Err("unavailable".into()),
            bound_role: Ok(None),
        }));
        assert!(matches!(
            failing
                .evaluate(Some("onebot"), Some(""), "native-user", Some("owner"))
                .await,
            Err(PermissionError::Store(_))
        ));

        let malformed = PermissionService::new(Arc::new(Store {
            settings: Ok(PermissionSettings {
                banned_users: Some(json!([123])),
            }),
            bound_role: Ok(None),
        }));
        assert!(matches!(
            malformed
                .evaluate(Some("onebot"), Some(""), "native-user", Some("owner"))
                .await,
            Err(PermissionError::Configuration(_))
        ));
    }
}
