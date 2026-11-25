//! 为角色声明插件扩展能力。
//!
//! 这些标识随 `/auth/status` 提供给界面。框架路由及资源范围继续使用
//! [`Capability`]，扩展接口自行校验调用者。

use std::sync::{Arc, OnceLock};

use super::{Capability, Role};

/// 为角色追加框架之外的能力标识。
///
/// 实现必须是纯函数：宿主在每次 `/auth/status` 都可能调用它，实现里做 IO 会把登录态
/// 查询拖成一串数据库往返。需要按账号细分的判断
/// 留在各自的业务接口里，不要塞进这里 —— 这里只回答「这个角色大致能看见什么」。
pub trait CapabilityProvider: Send + Sync {
    /// 声明 `role` 额外拥有的能力标识。
    ///
    /// 返回 `&'static str` 而非 `String`：这些名字要进响应体和审计日志，动态拼接的名字
    /// 会让「谁被授予了什么」变得无法枚举，也让下面的保留字检查失去意义。
    fn granted(&self, role: Role) -> Vec<&'static str>;
}

/// 注册扩展能力失败的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityProviderError {
    /// 已经有实现注册过了。
    ///
    /// 与 [`crate::entitlement`] 同理：能力清单决定界面入口，运行中热替换会出现
    /// 「刷新前看得到、刷新后看不到」的抖动，排查时没有任何日志能解释。
    #[error("能力扩展点已注册，不能重复设置")]
    AlreadyRegistered,
    /// 声明的名字与框架能力重名。
    ///
    /// 不禁止的话，插件可以用 `manage_runtime` 拿到与管理员同名、但走另一条判断路径的
    /// 能力标识，前端按字符串比较就会把管理入口渲染出来 —— 接口仍然会 403，但界面
    /// 开始撒谎。
    #[error("能力标识 `{0}` 是框架保留字")]
    Reserved(&'static str),
    /// 名字不是合法的 snake_case 标识。
    ///
    /// 前端拿这串字符直接比较，混进空格、大写或标点会让「明明授予了却不显示」变成
    /// 一次纯粹的拼写排查。
    #[error("能力标识 `{0}` 必须是 snake_case（小写字母、数字、下划线）")]
    InvalidName(&'static str),
}

static PROVIDER: OnceLock<Arc<dyn CapabilityProvider>> = OnceLock::new();

/// 注入扩展能力来源。只允许注册一次。
///
/// 失败时返回原因而不是静默忽略：插件在 `on_load` 里注册，重复注册通常意味着
/// 同一个插件被加载了两遍，那件事本身就该被看见。
pub fn set_capability_provider(
    provider: impl CapabilityProvider + 'static,
) -> Result<(), CapabilityProviderError> {
    // 先校验再占坑：一个非法实现不该把位置占掉，让后来正确的实现注册不进去。
    validate_provider(&provider)?;
    PROVIDER
        .set(Arc::new(provider))
        .map_err(|_| CapabilityProviderError::AlreadyRegistered)
}

pub(crate) fn validate_provider(
    provider: &dyn CapabilityProvider,
) -> Result<(), CapabilityProviderError> {
    for role in [Role::Owner, Role::Admin, Role::User] {
        for name in provider.granted(role) {
            validate_name(name)?;
        }
    }
    Ok(())
}

/// 框架能力之外、`role` 额外持有的能力标识（已剔除与框架重名的项）。
pub fn extension_capabilities(role: Role) -> Vec<&'static str> {
    let mut result = collect(PROVIDER.get().map(|provider| provider.as_ref()), role);
    for name in crate::plugin_extensions::capabilities(role) {
        if validate_name(name).is_ok() && !result.contains(&name) {
            result.push(name);
        }
    }
    result
}

/// 合并逻辑本身不碰全局，单元测试可以确定地断言「没注册就没有」，
/// 不必和注册用例抢进程级的 [`PROVIDER`]。
fn collect(provider: Option<&dyn CapabilityProvider>, role: Role) -> Vec<&'static str> {
    let Some(provider) = provider else {
        return Vec::new();
    };
    let mut granted: Vec<&'static str> = Vec::new();
    for name in provider.granted(role) {
        // 注册时已经校验过一遍，这里再挡一次：实现可以按调用次数返回不同结果
        // （比如内部计数器），注册那一刻合法不代表之后一直合法。
        if validate_name(name).is_err() || granted.contains(&name) {
            continue;
        }
        granted.push(name);
    }
    granted
}

/// `role` 是否被扩展点授予了 `name`。
///
/// 供插件在自己的 handler 里判断。框架自己的授权不走这里。
pub fn has_extension_capability(role: Role, name: &str) -> bool {
    extension_capabilities(role).contains(&name)
}

fn validate_name(name: &'static str) -> Result<(), CapabilityProviderError> {
    if !is_snake_case(name) {
        return Err(CapabilityProviderError::InvalidName(name));
    }
    if Capability::ALL.iter().any(|c| c.as_str() == name) {
        return Err(CapabilityProviderError::Reserved(name));
    }
    Ok(())
}

fn is_snake_case(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('_')
        && !name.ends_with('_')
        && !name.contains("__")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Commercial;

    impl CapabilityProvider for Commercial {
        fn granted(&self, role: Role) -> Vec<&'static str> {
            match role {
                Role::User => vec!["view_own_report"],
                _ => vec!["view_own_report", "manage_reports"],
            }
        }
    }

    struct Collides;

    impl CapabilityProvider for Collides {
        fn granted(&self, _role: Role) -> Vec<&'static str> {
            vec![Capability::ManageRuntime.as_str()]
        }
    }

    struct Malformed;

    impl CapabilityProvider for Malformed {
        fn granted(&self, _role: Role) -> Vec<&'static str> {
            vec!["View Own Billing"]
        }
    }

    #[test]
    fn unregistered_provider_grants_nothing() {
        // 开源构建的默认：不注册就一个扩展能力都没有，前端导航里也就没有扩展入口。
        for role in [Role::Owner, Role::Admin, Role::User] {
            assert!(
                collect(None, role).is_empty(),
                "{role} 不该凭空拿到扩展能力"
            );
        }
    }

    #[test]
    fn names_must_be_snake_case_and_not_reuse_framework_capabilities() {
        assert_eq!(
            set_capability_provider(Collides),
            Err(CapabilityProviderError::Reserved("manage_runtime"))
        );
        assert_eq!(
            set_capability_provider(Malformed),
            Err(CapabilityProviderError::InvalidName("View Own Billing"))
        );
        // 两次失败都不该占坑：合法实现稍后仍能注册。
        assert!(collect(Some(&Collides), Role::User).is_empty());
    }

    /// 合并时不信任实现的返回值：注册后仍要挡保留字和重复项。
    #[test]
    fn collection_filters_reserved_and_duplicate_names() {
        struct Sloppy;

        impl CapabilityProvider for Sloppy {
            fn granted(&self, _role: Role) -> Vec<&'static str> {
                vec!["view_own_report", "manage_runtime", "view_own_report"]
            }
        }

        assert_eq!(collect(Some(&Sloppy), Role::User), vec!["view_own_report"]);
    }

    #[test]
    fn snake_case_accepts_the_shapes_frontends_compare() {
        for name in ["view_own_report", "manage_reports", "reports2", "a_b_c"] {
            assert!(is_snake_case(name), "{name} 应被接受");
        }
        for name in [
            "",
            "_lead",
            "trail_",
            "double__underscore",
            "Upper",
            "with space",
        ] {
            assert!(!is_snake_case(name), "{name} 应被拒绝");
        }
    }

    #[test]
    fn framework_capability_strings_are_all_reserved() {
        for capability in Capability::ALL {
            assert_eq!(
                validate_name(capability.as_str()),
                Err(CapabilityProviderError::Reserved(capability.as_str()))
            );
        }
    }

    #[test]
    fn registration_grants_per_role_and_is_one_shot() {
        // 独占的注册用例：`PROVIDER` 是进程级的，这个测试必须在没人注册过时先跑赢。
        // 单测默认多线程并行，所以注册成功后立刻把断言做完，不留窗口给其它用例观察。
        assert_eq!(set_capability_provider(Commercial), Ok(()));
        assert_eq!(extension_capabilities(Role::User), vec!["view_own_report"]);
        assert_eq!(
            extension_capabilities(Role::Owner),
            vec!["view_own_report", "manage_reports"]
        );
        assert!(has_extension_capability(Role::Admin, "manage_reports"));
        assert!(!has_extension_capability(Role::User, "manage_reports"));
        assert_eq!(
            set_capability_provider(Commercial),
            Err(CapabilityProviderError::AlreadyRegistered)
        );
    }
}
