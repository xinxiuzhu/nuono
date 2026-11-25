//! Nuono 核心应用与各平台适配器共用的跨平台领域模型定义。
//!
//! 本 crate 定义了身份标识、消息段、事件、出站动作及其合法性校验规则，
//! 不依赖异步运行时、存储层、HTTP 服务或具体平台协议。
//! 平台协议归一化与运行时动作结构转换由上层集成模块负责。

#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    clippy::unwrap_used,
    clippy::exhaustive_enums,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::allow_attributes_without_reason
)]

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DomainModelError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} contains an ASCII control character")]
    AsciiControl { field: &'static str },
    #[error("{field} exceeds the maximum length of {max} bytes")]
    TooLong { field: &'static str, max: usize },
    #[error("{field} is not a valid {expected}")]
    InvalidValue {
        field: &'static str,
        expected: &'static str,
    },
    #[error("{field} is required for {platform}")]
    MissingNamespace {
        field: &'static str,
        platform: &'static str,
    },
    #[error("{field} must use the same platform as the account identity")]
    PlatformMismatch { field: &'static str },
}

pub mod action;
pub mod button;
pub use button::{ButtonAction, ButtonOne, ButtonPermission, ButtonStyle};
pub mod database;
pub mod event;
pub mod identity;
pub mod message;
pub mod scalar;

pub use action::{
    ActionError, ActionErrorKind, ActionId, ActionReceipt, ActionReceiptStatus, Capability,
    CapabilitySet, CapabilitySupport, CorrelationId, DeliveryTarget, ModerationOperation,
    OutboundAction, QueryOperation,
};
pub use event::{DomainEvent, DomainEventKind, DomainMessage, SessionKey, SessionScope};
pub use identity::{
    BotIdentity, ConversationRef, IdentityKind, IdentityRef, IdentityScheme, NativeId, Platform,
};
pub use message::{FileSource, MessageSegment, MessageTarget, UserType};

pub(crate) fn validate_text(value: &str, field: &'static str) -> Result<(), DomainModelError> {
    if value.trim().is_empty() {
        return Err(DomainModelError::Empty { field });
    }
    if value.chars().any(|character| character.is_ascii_control()) {
        return Err(DomainModelError::AsciiControl { field });
    }
    if value.len() > 512 {
        return Err(DomainModelError::TooLong { field, max: 512 });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_id_roundtrips_without_normalizing() {
        let id = NativeId::new("  account  ").unwrap();
        assert_eq!(id.as_str(), "  account  ");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""  account  ""#);
        let restored: NativeId = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, id);
    }

    #[test]
    fn native_id_rejects_empty_control_and_long_values() {
        assert!(NativeId::new("   ").is_err());
        assert!(NativeId::new("ok\nvalue").is_err());
        assert!(NativeId::new("x".repeat(513)).is_err());
    }
}

pub mod deployment;
