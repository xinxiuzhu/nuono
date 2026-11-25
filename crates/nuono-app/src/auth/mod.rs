//! 与 HTTP 层和数据库模型解耦的账号认证与权限规则。
mod capability;
mod extension;
pub(crate) use extension::validate_provider;
mod identity;
mod password;
mod role;
mod scope;
pub use capability::Capability;
pub use extension::{
    CapabilityProvider, CapabilityProviderError, extension_capabilities, has_extension_capability,
    set_capability_provider,
};
pub use identity::{AccountIdentity, CallerIdentity};
pub use password::{
    MAX_PASSWORD_LEN, MIN_PASSWORD_LEN, password_length_message, validate_password,
};
pub use role::{Role, UnknownRole};
pub use scope::Scope;

mod totp;
pub use totp::{
    RECOVERY_CODE_COUNT, generate_recovery_codes, generate_secret, otpauth_uri, recovery_code_hash,
    validate_code, validate_code_counter,
};

/// 常数时间比较两个字节串。
///
/// 用于一切「请求提供的凭据 vs 存储的期望值」比较（token、签名、哈希）：逐字节
/// 短路比较会泄露前缀匹配长度。长度不等直接返回 false——长度本身不是秘密。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

mod validation;
pub use validation::*;
