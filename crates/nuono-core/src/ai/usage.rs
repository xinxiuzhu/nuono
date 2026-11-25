//! 为模型调用挂载宿主侧用量持久化记录，不记录任何密钥凭据或请求正文。
use crate::{config::AIConfig, db::Database};
use nuono_ai::usage::UsageContext;
use nuono_ai_provider::{OpenAiCompatibleProvider, usage::UsageTracking};

pub(crate) fn tracking(db: &Database, context: UsageContext) -> UsageTracking {
    UsageTracking {
        observer: db.ai_usage_observer(),
        context,
    }
}
pub(crate) fn model_context(model: &crate::entity::ai_models::Model, source: &str) -> UsageContext {
    UsageContext {
        model_id: Some(model.id),
        name: model.name.clone(),
        provider: model.provider.clone(),
        source: source.into(),
    }
}
pub(crate) fn legacy_context(ai: &AIConfig, source: &str) -> UsageContext {
    UsageContext {
        model_id: None,
        name: ai.model.clone(),
        provider: ai.provider.clone(),
        source: source.into(),
    }
}
pub(crate) fn from_config(
    config: &nuono_ai::AiConfig,
    db: &Database,
) -> Result<OpenAiCompatibleProvider, nuono_ai::AiError> {
    let context = UsageContext {
        model_id: config
            .extra
            .get("nuono.usage.model_id")
            .and_then(|id| id.parse().ok()),
        name: config
            .extra
            .get("nuono.usage.name")
            .cloned()
            .unwrap_or_else(|| config.model.clone()),
        provider: config
            .extra
            .get("nuono.usage.provider")
            .cloned()
            .unwrap_or_else(|| config.provider.clone()),
        source: "agent".into(),
    };
    OpenAiCompatibleProvider::from_config(config)
        .map(|provider| provider.with_usage_observer(tracking(db, context)))
}

#[cfg(test)]
#[path = "../../tests/unit/ai/usage_tests.rs"]
mod tests;
