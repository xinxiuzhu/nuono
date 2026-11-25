//! 运行时语言装配。语言偏好不得改变路由或原生身份标识。
use std::sync::Arc;

use nuono_app::localization::{
    Locale, LocaleContext, LocaleScope, LocaleService, SystemLocaleProvider,
};

use crate::{ConfigManager, Database, Result};

struct ConfigLocaleProvider(Arc<ConfigManager>);

#[async_trait::async_trait]
impl SystemLocaleProvider for ConfigLocaleProvider {
    async fn system_locale(&self) -> Result<Locale> {
        Ok(self.0.snapshot().await.localization.default_locale)
    }
}

pub fn service(db: Database, config: Arc<ConfigManager>) -> LocaleService {
    with_config(LocaleService::new(Arc::new(db)), config)
}

pub(crate) fn with_config(locales: LocaleService, config: Arc<ConfigManager>) -> LocaleService {
    locales.with_system_locale_provider(Arc::new(ConfigLocaleProvider(config)))
}

pub(crate) fn event_scopes(
    event: &serde_json::Value,
    domain: Option<&crate::domain::DomainEvent>,
) -> Result<(Option<LocaleScope>, Option<LocaleScope>)> {
    let event = nuono_plugin::Event::with_domain(event, domain);
    Ok((
        event
            .user_locale_scope()
            .map_err(|error| crate::NuonoError::Validation(error.to_string()))?,
        event
            .conversation_locale_scope()
            .map_err(|error| crate::NuonoError::Validation(error.to_string()))?,
    ))
}

pub(crate) async fn resolve_event(
    locales: &LocaleService,
    event: &serde_json::Value,
    domain: Option<&crate::domain::DomainEvent>,
) -> Result<LocaleContext> {
    let (user, conversation) = event_scopes(event, domain)?;
    locales
        .resolve(None, user.as_ref(), conversation.as_ref())
        .await
}

/// 语言偏好读取失败只影响展示，绝不参与任何授权判定。
pub(crate) async fn resolve_event_or_default(
    locales: &LocaleService,
    event: &serde_json::Value,
    domain: Option<&crate::domain::DomainEvent>,
) -> LocaleContext {
    match resolve_event(locales, event, domain).await {
        Ok(locale) => locale,
        Err(error) => {
            tracing::warn!(%error, "语言偏好读取失败，使用系统语言");
            LocaleContext {
                locale: locales.system_locale().await.unwrap_or_default(),
                source: nuono_app::localization::LocaleSource::System,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_app::localization::LocaleSource;

    #[tokio::test]
    async fn locale_background_handle_reads_new_system_default_without_mutating_old_snapshot() {
        let config = Arc::new(ConfigManager::for_test(
            crate::config::NuonoConfig::default(),
            "unused-localization.toml".into(),
        ));
        let service = with_config(LocaleService::default(), config.clone());
        let background = service.clone();
        let old = service.resolve(None, None, None).await.unwrap();
        assert_eq!(old.locale, Locale::ZhCn);
        let mut next = config.get().await;
        next.localization.default_locale = Locale::Ja;
        config.replace_runtime(next).await.unwrap();
        assert_eq!(
            background.resolve(None, None, None).await.unwrap(),
            LocaleContext {
                locale: Locale::Ja,
                source: LocaleSource::System
            }
        );
        assert_eq!(old.locale, Locale::ZhCn);
    }
}
