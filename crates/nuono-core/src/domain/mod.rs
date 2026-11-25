//! 宿主层的领域模型重导出门面与平台事件归一化入口。

pub mod action;
pub mod normalizer;
pub mod projection;

pub use action::LegacyProjectionError;
pub use normalizer::DomainEventNormalizer;
pub use nuono_domain::{
    ActionError, ActionErrorKind, ActionId, ActionReceipt, ActionReceiptStatus, BotIdentity,
    Capability, CapabilitySet, CapabilitySupport, ConversationRef, CorrelationId, DeliveryTarget,
    DomainEvent, DomainEventKind, DomainMessage, DomainModelError, FileSource, IdentityKind,
    IdentityRef, IdentityScheme, MessageSegment, MessageTarget, ModerationOperation, NativeId,
    OutboundAction, Platform, QueryOperation, SessionKey, SessionScope, UserType, event, identity,
    message,
};
pub use projection::project_legacy_runtime_event;
