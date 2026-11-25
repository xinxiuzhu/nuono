//! 面向与宿主无关的插件 API 的模块重导出。
pub use nuono_plugin::api::{bot, button, routed, segment, session, waiter};

pub use bot::{Bot, MessageContext, MessageTarget};
pub use button::{ButtonAction, ButtonOne, ButtonPermission, ButtonStyle};
pub use routed::{RuntimeMessageTarget, RuntimeTargetKind};
pub use segment::{MessageBuilder, MessageSegment};
pub use session::{InteractiveSession, SessionManager};
pub use waiter::WaitManager;
