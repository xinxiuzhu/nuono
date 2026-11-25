//! 数据可见范围。
//!
//! 能力回答「能不能进这个接口」，Scope 回答「在接口里能看到哪些行」。两者必须都有：
//! 只有能力，`GET /groups` 会把全站群返回给每个客户；只有 Scope，客户能进
//! `POST /plugins/reload` 这类根本没有归属维度的接口。
//!
//! **边界（很重要）**：Scope 只作用于 API 的读写路径。运行时派发 —— 调度器
//! （`core/scheduler.rs`）、事件订阅（`core/subscribe.rs`）、bot 加载、消息过滤
//! （`core/filter.rs`）—— 一律以系统身份执行，不套 Scope。否则一个用户停用自己的账号
//! 就会连带停掉别人的定时任务，因为那些循环并不属于任何请求上下文。

use super::capability::Capability;
use super::role::Role;

/// 请求能触达的数据范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 不受归属限制。仅授予管理侧角色（`Owner` / `Admin`）。
    All,
    /// 只能触达当前用户被授权或归属的资源行。
    ///
    /// 对群组资源而言，普通用户的可见与管理权限来自 `group_grants`（群授权记录）或已验证的
    /// 平台管理员身份；未获授权的资源**不在**范围内。对个人归属资源（如个人订阅、个人凭据），
    /// 只匹配 `user_id` 等于该值的行。
    OwnedBy(i64),
}

impl Scope {
    /// 由「跨归属能力」推导范围，而不是匹配角色名。
    ///
    /// 用 `role.is_staff()` 判断会让范围和能力成为两套独立事实：将来给某个角色收回
    /// `ViewAllGroups`，它照样能拿到 `Scope::All`，收权就成了假的。这里只问一个问题 ——
    /// 该角色有没有跨归属能力 —— 范围便永远跟着能力走。
    ///
    /// `capability` 传的是本次操作对应的跨归属能力：读取传 [`Capability::ViewAllGroups`]，
    /// 写入传 [`Capability::ManageAllGroups`]。分开传是为了让「能看全部但只能改自己的」
    /// 这种组合可表达，而不是一个布尔一刀切。
    pub fn for_capability(role: Role, capability: Capability, user_id: i64) -> Self {
        if role.has(capability) {
            Self::All
        } else {
            Self::OwnedBy(user_id)
        }
    }

    /// 该范围是否覆盖某条资源的归属值。
    ///
    /// `owner` 为 `None`（无主）时只有 [`Scope::All`] 通过。
    pub fn covers(self, owner: Option<i64>) -> bool {
        match self {
            Self::All => true,
            Self::OwnedBy(uid) => owner == Some(uid),
        }
    }

    /// 需要按归属过滤时返回用户 id，不受限时返回 `None`。
    ///
    /// 给查询构造用：`if let Some(uid) = scope.owner_filter() { query.filter(...) }`。
    pub fn owner_filter(self) -> Option<i64> {
        match self {
            Self::All => None,
            Self::OwnedBy(uid) => Some(uid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staff_sees_everything_and_customers_see_only_their_own() {
        for capability in [Capability::ViewAllGroups, Capability::ManageAllGroups] {
            assert_eq!(
                Scope::for_capability(Role::Owner, capability, 1),
                Scope::All
            );
            assert_eq!(
                Scope::for_capability(Role::Admin, capability, 2),
                Scope::All
            );
            assert_eq!(
                Scope::for_capability(Role::User, capability, 7),
                Scope::OwnedBy(7),
                "客户没有跨归属能力，必须被收窄到自己名下"
            );
        }
    }

    /// 范围跟着能力走，不跟着角色名走。
    ///
    /// 传一个客户和管理侧都不持有的能力，管理侧也应被收窄 —— 这证明 `for_capability`
    /// 查的是能力而不是在里面偷偷判角色。
    #[test]
    fn scope_follows_capability_not_role_name() {
        assert_eq!(
            Scope::for_capability(Role::Owner, Capability::ViewGroups, 1),
            Scope::All,
            "owner 持有 ViewGroups"
        );
        assert_eq!(
            Scope::for_capability(Role::Admin, Capability::AssignRoles, 2),
            Scope::OwnedBy(2),
            "admin 没有 AssignRoles，就该被收窄"
        );
    }

    /// 无主资源不属于任何客户 —— 这是「未归属的群谁都不能动」那条规则的落点。
    #[test]
    fn unowned_rows_are_out_of_customer_scope() {
        assert!(!Scope::OwnedBy(7).covers(None));
        assert!(Scope::All.covers(None));
    }

    #[test]
    fn customer_scope_rejects_other_owners() {
        let scope = Scope::OwnedBy(7);
        assert!(scope.covers(Some(7)));
        assert!(!scope.covers(Some(8)));
        assert_eq!(scope.owner_filter(), Some(7));
        assert_eq!(Scope::All.owner_filter(), None);
    }
}
