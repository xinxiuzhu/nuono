//! 单次 API 请求的调用者身份。

use super::capability::Capability;
use super::role::Role;
use super::scope::Scope;
/// 存储层返回的账号基础身份投影。
/// 认证中间件必须在构造 `CallerIdentity` 前回查数据库获取最新角色。
pub trait AccountIdentity {
    fn account_id(&self) -> i64;
    fn account_username(&self) -> &str;
    fn account_role(&self) -> &str;
}

/// 已认证请求的调用者。
///
/// 由中间件在会话校验通过后注入请求扩展，handler 通过 `Extension<CallerIdentity>` 取用。
///
/// 角色是每次请求回查 `users` 表得到的，不缓存在会话载荷里：会话是长期凭据，而 owner
/// 随时可能降级某个账号。缓存角色会让降级要等到会话过期才生效。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerIdentity {
    pub user_id: i64,
    pub username: String,
    pub role: Role,
}

impl CallerIdentity {
    pub fn new(user_id: i64, username: impl Into<String>, role: Role) -> Self {
        Self {
            user_id,
            username: username.into(),
            role,
        }
    }

    /// 从已校验的账号行构造。
    ///
    /// 角色走 [`Role::from_db_value`]，未知文本退到最低权限而不是拒绝请求 —— 拒绝会把
    /// 一行脏数据放大成该账号完全不可用。
    pub fn from_user(user: &impl AccountIdentity) -> Self {
        Self::new(
            user.account_id(),
            user.account_username(),
            Role::from_db_value(user.account_role()),
        )
    }

    /// 是否拥有某项能力。授权判断只用这个函数。
    pub fn has(&self, capability: Capability) -> bool {
        self.role.has(capability)
    }

    /// 是否是 owner。
    ///
    /// 只用于日志和展示。想表达「只有 owner 能做」请用
    /// [`Capability::AssignRoles`] 之类的具名能力 —— 那样新增 owner 专属操作时，
    /// 授权规则集中在一处可审计，而不是散成一堆 `is_owner()` 判断。
    pub fn is_owner(&self) -> bool {
        self.role == Role::Owner
    }

    /// 是否属于框架管理侧。措辞分流用，不做权限闸门，理由同 [`Role::is_staff`]。
    pub fn is_staff(&self) -> bool {
        self.role.is_staff()
    }

    /// 该调用者的数据可见范围。
    ///
    /// 必须传入本次操作对应的跨归属能力（读取传 [`Capability::ViewAllGroups`]，写入传
    /// [`Capability::ManageAllGroups`]），不提供无参版本：无参就得在内部猜是读还是写，
    /// 猜错的方向恰好是把写操作放宽成全站。参数强制调用点说清意图。
    pub fn scope(&self, cross_owner: Capability) -> Scope {
        if self.has(cross_owner) {
            Scope::All
        } else {
            Scope::OwnedBy(self.user_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(role: Role) -> CallerIdentity {
        CallerIdentity::new(42, "orion", role)
    }

    /// 范围随传入的跨归属能力变化，并且带上调用者自己的 id。
    #[test]
    fn scope_narrows_customers_and_opens_up_for_staff() {
        for cross_owner in [Capability::ViewAllGroups, Capability::ManageAllGroups] {
            assert_eq!(caller(Role::User).scope(cross_owner), Scope::OwnedBy(42));
            assert_eq!(caller(Role::Admin).scope(cross_owner), Scope::All);
            assert_eq!(caller(Role::Owner).scope(cross_owner), Scope::All);
        }
    }

    #[test]
    fn capability_check_delegates_to_role() {
        assert!(caller(Role::User).has(Capability::ManageOwnAccount));
        assert!(!caller(Role::User).has(Capability::ViewObservability));
        assert!(caller(Role::Owner).has(Capability::AssignRoles));
        assert!(!caller(Role::Admin).has(Capability::AssignRoles));
    }

    #[test]
    fn owner_flag_is_exact_not_staff() {
        assert!(caller(Role::Owner).is_owner());
        assert!(!caller(Role::Admin).is_owner(), "admin 不是 owner");
        assert!(caller(Role::Admin).is_staff());
        assert!(!caller(Role::User).is_staff());
    }
}
