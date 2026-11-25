//! 能力：授权判断的唯一词汇。
//!
//! 之前授权是「这个路由要不要 admin」的布尔判断，散在中间件白名单和三处 handler 里。
//! 问题是布尔值没有中间态：既表达不了「owner 能做但 admin 不能」（建 admin 账号就是
//! 这种），也表达不了「用户能做但只对自己的数据」。这里把权限拆成有名字的能力，
//! 角色到能力的映射只有一处（[`Role::capabilities`]），路由和 handler 都只问能力。
//!
//! 加新能力的规矩：先在这里加变体，编译器会在 [`Role::capabilities`] 的 match 里
//! 报错，逼你显式决定三个角色各自给不给 —— 而不是默默继承某个角色的现有权限。

use std::fmt;

use serde::{Deserialize, Serialize};

use super::role::Role;

/// 一个可授予的操作类别。
///
/// 粒度按「谁能做」划分，不按路由划分：同一能力可以管多个端点，但一个端点只对应一个能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    // 所有登录账号
    /// 读写自己的账号资料：用户名、邮箱、密码、头像。
    ManageOwnAccount,
    /// 读取前端外壳渲染所需的公共信息（品牌、主题）。
    ViewPublicShell,

    // 资源能力（可达性；具体能看到哪些行由 Scope 决定）
    //
    // 这几项刻意**不**叫 `…OwnGroups`：能力回答的是「这个接口够不够得着」，而「够得着之后
    // 看到谁的数据」是 [`super::Scope`] 的职责（成员 `OwnedBy(id)`，管理侧 `All`）。
    // 早先把两件事塞进一个能力，导致管理员反而读不了群列表 —— 他们没有「自己的群」。
    /// 查看群列表、成员与活跃数据。行范围由 Scope 收窄。
    ViewGroups,
    /// 管理群的运行开关与插件配置。写入前逐个校验归属。
    ManageGroups,

    // 管理能力（跨归属，普通成员永远拿不到）
    /// 跨归属查看：不受 Scope 限制地读取任意群，含无主群。
    ViewAllGroups,
    /// 跨归属管理：群归属指派、bot 级全量同步。
    ManageAllGroups,
    /// 管理框架运行配置、插件、适配器、机器人实例。
    ManageRuntime,
    /// 查看运行可观测数据：日志流、Prometheus 指标、事件审计。
    ViewObservability,
    /// 管理账号：建号、停用、改邮箱。
    ManageUsers,

    // Owner 专属
    /// 变更他人角色，含创建 admin。
    ///
    /// 只给 owner：admin 之间互相建号等于让任一 admin 无限扩权，一个被盗的 admin
    /// 会话就能自我复制出永久驻留的后备账号。
    AssignRoles,
}

impl Capability {
    /// 全部能力，供穷举测试使用。
    ///
    /// 加新变体时这里也要加 —— 漏加会让「owner 持有全部能力」这条测试假绿。顺序无意义。
    pub const ALL: &'static [Capability] = &[
        Self::ManageOwnAccount,
        Self::ViewPublicShell,
        Self::ViewGroups,
        Self::ManageGroups,
        Self::ViewAllGroups,
        Self::ManageAllGroups,
        Self::ManageRuntime,
        Self::ViewObservability,
        Self::ManageUsers,
        Self::AssignRoles,
    ];

    /// 稳定的文本标识，用于审计日志和拒绝时的指标标签。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ManageOwnAccount => "manage_own_account",
            Self::ViewPublicShell => "view_public_shell",
            Self::ViewGroups => "view_groups",
            Self::ManageGroups => "manage_groups",
            Self::ViewAllGroups => "view_all_groups",
            Self::ManageAllGroups => "manage_all_groups",
            Self::ManageRuntime => "manage_runtime",
            Self::ViewObservability => "view_observability",
            Self::ManageUsers => "manage_users",
            Self::AssignRoles => "assign_roles",
        }
    }

    /// 面向终端用户的中文说明，用于 403 文案。
    ///
    /// 措辞刻意不提「管理员」：普通用户碰到管理接口是正常的导航行为，不该被告知
    /// 「你不是管理员」，那既没信息量又暗示存在可提权的目标。
    pub const fn denial_message(self) -> &'static str {
        match self {
            Self::ViewAllGroups | Self::ManageAllGroups => "该操作仅限你名下的群",
            Self::AssignRoles => "只有所有者能变更账号角色",
            _ => "当前账号无权访问该功能",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Role {
    /// 该角色拥有的能力全集。
    ///
    /// 三个角色各写一份完整列表，而不是「admin = user + 若干」的累加式定义。累加式写法会让
    /// 「给用户加一项能力」自动漏给管理员，反过来也看不出差异。列表冗余一点，换取每次改动
    /// 都必须显式落到某个角色上、在 diff 里看得见。
    ///
    /// 资源能力（`ViewGroups`/`ManageGroups`）三个角色都有 —— 它只决定接口可达。
    /// 管理侧额外持有 `ViewAllGroups`/`ManageAllGroups`，那两项才是「跨归属」的开关，
    /// 由 [`crate::auth::Scope::for_capability`] 翻译成 SQL 层的行过滤。
    pub fn capabilities(self) -> &'static [Capability] {
        use Capability::*;

        match self {
            Role::Owner => &[
                ManageOwnAccount,
                ViewPublicShell,
                ViewGroups,
                ManageGroups,
                ViewAllGroups,
                ManageAllGroups,
                ManageRuntime,
                ViewObservability,
                ManageUsers,
                AssignRoles,
            ],
            Role::Admin => &[
                ManageOwnAccount,
                ViewPublicShell,
                ViewGroups,
                ManageGroups,
                ViewAllGroups,
                ManageAllGroups,
                ManageRuntime,
                ViewObservability,
                ManageUsers,
            ],
            Role::User => &[ManageOwnAccount, ViewPublicShell, ViewGroups, ManageGroups],
        }
    }

    /// 该角色是否拥有某项能力。
    pub fn has(self, capability: Capability) -> bool {
        self.capabilities().contains(&capability)
    }
}

#[cfg(test)]
mod tests {
    use super::Capability::*;
    use super::*;

    /// 普通用户不能拿到任何管理能力。这条是本次重构的核心不变量。
    #[test]
    fn plain_user_holds_no_management_capability() {
        for capability in [
            ViewAllGroups,
            ManageAllGroups,
            ManageRuntime,
            ViewObservability,
            ManageUsers,
            AssignRoles,
        ] {
            assert!(!Role::User.has(capability), "普通用户不应拥有 {capability}");
        }
    }

    /// 角色变更只有 owner 能做，admin 也不行。
    #[test]
    fn only_owner_assigns_roles() {
        assert!(Role::Owner.has(AssignRoles));
        assert!(!Role::Admin.has(AssignRoles));
        assert!(!Role::User.has(AssignRoles));
    }

    /// 可达性和跨归属是两件事：三种角色都够得着群接口，只有管理侧能跨归属。
    ///
    /// 这条用例是一次真实回归的守卫。我最初把这两件事塞进一个能力（`ViewOwnGroups`），
    /// 管理侧按设计不持有它，结果 `GET /api/groups` 对 owner 和 admin 直接 403 ——
    /// 他们没有「自己的群」。运行时矩阵抓到了，单元测试当时反而在为错误模型背书。
    #[test]
    fn every_role_can_reach_groups_but_only_staff_crosses_ownership() {
        for role in [Role::Owner, Role::Admin, Role::User] {
            assert!(role.has(ViewGroups), "{role} 应够得着群列表接口");
            assert!(role.has(ManageGroups), "{role} 应够得着群管理接口");
        }

        for role in [Role::Owner, Role::Admin] {
            assert!(role.has(ViewAllGroups), "{role} 应能跨归属读取");
            assert!(role.has(ManageAllGroups), "{role} 应能跨归属管理");
        }

        assert!(!Role::User.has(ViewAllGroups), "客户不得跨归属读取");
        assert!(!Role::User.has(ManageAllGroups), "客户不得跨归属管理");
    }

    /// 三种角色都能管自己的账号 —— 头像、密码、邮箱不是管理员特权。
    #[test]
    fn every_role_manages_its_own_account() {
        for role in [Role::Owner, Role::Admin, Role::User] {
            assert!(role.has(ManageOwnAccount));
            assert!(role.has(ViewPublicShell));
        }
    }

    /// owner 是 admin 的超集：反过来会出现 admin 能做而 owner 不能的洞。
    #[test]
    fn owner_is_a_superset_of_admin() {
        for capability in Role::Admin.capabilities() {
            assert!(Role::Owner.has(*capability), "owner 缺少 {capability}");
        }
    }

    /// 能力列表里不该有重复项，`as_str` 也不该有两个变体撞同一个标识 ——
    /// 撞了会让审计和指标把两种拒绝混成一条。
    #[test]
    fn capability_identifiers_are_unique() {
        for role in [Role::Owner, Role::Admin, Role::User] {
            let mut seen = std::collections::HashSet::new();
            for capability in role.capabilities() {
                assert!(
                    seen.insert(*capability),
                    "{role} 的能力列表重复: {capability}"
                );
            }
        }

        let all = [
            ManageOwnAccount,
            ViewPublicShell,
            ViewGroups,
            ManageGroups,
            ViewAllGroups,
            ManageAllGroups,
            ManageRuntime,
            ViewObservability,
            ManageUsers,
            AssignRoles,
        ];
        let names: std::collections::HashSet<_> = all.iter().map(|c| c.as_str()).collect();
        assert_eq!(names.len(), all.len(), "存在重名的能力标识");
    }
}
