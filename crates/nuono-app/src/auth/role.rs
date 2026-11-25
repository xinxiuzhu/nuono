//! 账号角色。
//!
//! 角色在数据库里仍然是文本列（`users.role`），但代码里不再传 `&str`：字符串让
//! `role == "admin"` 这类判断散落在各个 handler 里，拼错一个字母就是静默放行。
//! 这里收成一个枚举，比较交给类型系统，落库时才转回文本。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 登录账号的角色。
///
/// `Owner` 是首启向导创建的唯一超级管理员，`Admin` 管理框架本身，`User` 是普通成员 ——
/// 只能管自己名下的群和自己的账号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    User,
}

impl Role {
    /// 落库用的文本值。`const fn` 是为了让 `db::users` 的 `ROLE_*` 常量能直接由它派生，
    /// 保证枚举和常量不可能漂移成两套值。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::User => "user",
        }
    }

    /// 解析数据库里的角色文本，无法识别时返回 `None`。
    ///
    /// 需要「拿不准就当最小权限」的场景用 [`Role::from_db_value`]，不要在这里做兜底 ——
    /// 校验用户输入（例如建号接口的 `role` 字段）必须能看到解析失败并回 400。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "user" => Some(Self::User),
            _ => None,
        }
    }

    /// 解析已落库的角色，无法识别时退到 [`Role::User`]。
    ///
    /// 数据库里出现未知角色只有两种来源：手工改库，或将来新增角色后回滚了代码。两种情况下
    /// 「当成最低权限」都是唯一安全的选择 —— 反过来（当成管理员，或直接 panic 让请求 500）
    /// 一个是提权，一个是把脏数据变成全站不可用。
    pub fn from_db_value(value: &str) -> Self {
        match Self::parse(value) {
            Some(role) => role,
            None => {
                tracing::warn!(role = %value, "数据库中存在无法识别的角色，按最低权限处理");
                Self::User
            }
        }
    }

    /// 是否属于框架管理侧（owner 或 admin）。
    ///
    /// 只用于「管理员 vs 客户」这种粗粒度分流（例如日志措辞）。授权判断一律走
    /// [`crate::auth::Capability`]，不要用这个函数当权限闸门 —— 那会让新增能力时
    /// 无法只给 admin 不给 owner，或反过来。
    pub const fn is_staff(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = UnknownRole;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| UnknownRole(value.to_string()))
    }
}

/// 角色文本无法识别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRole(pub String);

impl fmt::Display for UnknownRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "不支持的角色: {}", self.0)
    }
}

impl std::error::Error for UnknownRole {}

#[cfg(test)]
mod tests {
    use super::Role;

    /// 枚举的文本值必须和数据库里已有的行完全一致，否则升级后所有存量账号都解析不出角色。
    #[test]
    fn text_values_match_stored_rows() {
        assert_eq!(Role::Owner.as_str(), "owner");
        assert_eq!(Role::Admin.as_str(), "admin");
        assert_eq!(Role::User.as_str(), "user");
    }

    #[test]
    fn parse_round_trips_every_variant() {
        for role in [Role::Owner, Role::Admin, Role::User] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
            assert_eq!(role.to_string().parse::<Role>().unwrap(), role);
        }
    }

    /// 未知角色必须落到最低权限，不能是 admin，也不能 panic。
    #[test]
    fn unknown_role_falls_back_to_least_privilege() {
        assert_eq!(Role::from_db_value("superuser"), Role::User);
        assert_eq!(Role::from_db_value(""), Role::User);
        assert_eq!(Role::from_db_value("OWNER"), Role::User, "角色大小写敏感");
        assert!(Role::parse("superuser").is_none(), "校验路径要能看到失败");
    }

    /// JSON 表示也要是同一套小写文本：前端和审计日志都按它比对。
    #[test]
    fn serializes_as_lowercase_text() {
        assert_eq!(
            serde_json::to_string(&Role::Owner).unwrap(),
            "\"owner\"",
            "序列化值变了会同时打断前端和存量审计记录"
        );
        assert_eq!(
            serde_json::from_str::<Role>("\"user\"").unwrap(),
            Role::User
        );
    }
}
