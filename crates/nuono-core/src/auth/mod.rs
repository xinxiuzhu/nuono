//! 授权内核：角色、能力、数据范围、调用者身份。
//!
//! 放在 `nuono-core` 而不是 `nuono-server`，是为了让 `service` 层的函数能直接接
//! [`Scope`] 参数。之前 `GroupScope` 定义在 `service/groups.rs` 里、`CallerIdentity`
//! 定义在 server 的中间件里，两者要靠 handler 手工桥接一次 —— 每个新接口都得记得桥，
//! 忘了就是全量返回。
//!
//! 三个概念的分工：
//!
//! - [`Role`] —— 账号是谁。数据库里的一列文本。
//! - [`Capability`] —— 能做什么类别的操作。中间件按它决定路由准入。
//! - [`Scope`] —— 能碰哪些行。service 层按它收窄查询。
//!
//! [`CallerIdentity`] 把三者绑到一个请求上。它只在 API 请求上下文中存在；运行时派发
//! （调度、事件、bot 加载）没有调用者，以系统身份执行，详见 `scope` 模块的说明。

pub use nuono_app::auth::constant_time_eq;

pub use nuono_app::auth::{
    AccountIdentity, CallerIdentity, Capability, MAX_PASSWORD_LEN, MIN_PASSWORD_LEN, Role, Scope,
    UnknownRole, password_length_message, validate_password,
};

/// 插件扩展能力：字符串标识，不进 [`Capability`] 闭集，只影响界面入口。
pub use nuono_app::auth::{
    CapabilityProvider, CapabilityProviderError, extension_capabilities, has_extension_capability,
    set_capability_provider,
};

pub use nuono_app::auth::{
    RECOVERY_CODE_COUNT, generate_recovery_codes, generate_secret, otpauth_uri, recovery_code_hash,
    validate_code, validate_code_counter,
};
