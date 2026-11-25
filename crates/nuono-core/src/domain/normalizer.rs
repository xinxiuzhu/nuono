//! 将入站统一事件分发给对应平台适配器归一化为 `DomainEvent` 的调度器。

use super::{BotIdentity, DomainEvent, DomainModelError, Platform};
use crate::core::platform::UnifiedEvent;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct DomainEventNormalizer {
    bot: BotIdentity,
}

impl DomainEventNormalizer {
    pub fn new(bot: BotIdentity) -> Self {
        Self { bot }
    }

    pub fn normalize(
        &self,
        event: &UnifiedEvent,
        received_at: DateTime<Utc>,
    ) -> Result<DomainEvent, DomainModelError> {
        if let Some(provider) = nuono_adapter::running().for_platform(self.bot.platform().as_str())
        {
            return provider.normalize(&self.bot, event, received_at);
        }
        // 沙箱直接构造标准 OneBot 身份 DTO，无需经过真实网络适配器。
        if self.bot.platform() == &Platform::OneBot {
            return nuono_app::platform::canonical::normalize_event(&self.bot, event, received_at);
        }
        Err(DomainModelError::InvalidValue {
            field: "bot.account_id.platform",
            expected: "an installed adapter",
        })
    }
}
