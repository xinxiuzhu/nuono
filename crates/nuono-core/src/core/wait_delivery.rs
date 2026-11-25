//! 常规事件管线与有界 OneBot 独立等待通道共用的 WaitReply 准入与投递逻辑。
use super::{
    filter::{FilterHandler, FilterResponse},
    handler::EventHandler,
    plugin_host::PluginHost,
};
use nuono_plugin::api::waiter::{WaitEvent, WaitManager, WaitSnapshot};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct WaitDelivery {
    waits: Arc<WaitManager>,
    filter: Arc<FilterHandler>,
    handler: Arc<EventHandler>,
    host: Arc<PluginHost>,
    core_commands: Arc<crate::core_commands::CoreCommands>,
}
impl WaitDelivery {
    pub(crate) fn new(
        waits: Arc<WaitManager>,
        filter: Arc<FilterHandler>,
        handler: Arc<EventHandler>,
        host: Arc<PluginHost>,
        core_commands: Arc<crate::core_commands::CoreCommands>,
    ) -> Self {
        Self {
            waits,
            filter,
            handler,
            host,
            core_commands,
        }
    }
    /// 返回 `true` 表示该事件已被等待会话消费（或因插件权限/开关校验未通过而被拦截）；
    /// 返回 `false` 表示未命中有效等待会话，事件将继续走常规派发（已过期或已关闭的等待注册绝不会吞掉无关消息）。
    pub(crate) async fn deliver(
        &self,
        bot_id: i64,
        event: &Value,
        domain: Option<&crate::domain::DomainEvent>,
        locale: nuono_app::localization::LocaleContext,
    ) -> bool {
        if event.get("post_type").and_then(Value::as_str) != Some("message") {
            return false;
        }
        let text = event
            .get("raw_message")
            .and_then(Value::as_str)
            .unwrap_or("");
        if self.core_commands.claims_message(text).await
            || self.host.message_route(text).await.is_explicit()
        {
            return false;
        }
        let Some(wait) = self.waits.snapshot_for_event(bot_id, event) else {
            return false;
        };
        self.deliver_registered(bot_id, &wait, event, domain, locale)
            .await
    }

    /// 交互按钮回复会校验当初注册时的等待批次号（generation）；
    /// 即使会话键（session_key）相同，旧按钮点击也无法回答后来新注册的替换等待会话。
    pub(crate) async fn deliver_registered(
        &self,
        bot_id: i64,
        wait: &WaitSnapshot,
        event: &Value,
        domain: Option<&crate::domain::DomainEvent>,
        locale: nuono_app::localization::LocaleContext,
    ) -> bool {
        let scope = match self.host.event_configuration_scope(domain).await {
            Ok(scope) => scope,
            Err(_) => return true,
        };
        nuono_plugin::config::with_runtime_configurations(
            scope,
            self.deliver_registered_scoped(bot_id, wait, event, domain, locale),
        )
        .await
    }
    async fn deliver_registered_scoped(
        &self,
        bot_id: i64,
        wait: &WaitSnapshot,
        event: &Value,
        domain: Option<&crate::domain::DomainEvent>,
        locale: nuono_app::localization::LocaleContext,
    ) -> bool {
        if event.get("post_type").and_then(Value::as_str) != Some("message") {
            return false;
        }
        if WaitManager::event_session_id(bot_id, event).as_deref() != Some(wait.session_id.as_str())
            || !self
                .waits
                .snapshot(&wait.session_id)
                .is_some_and(|current| current.generation == wait.generation)
        {
            return false;
        }
        let admitted = self
            .allowed(
                event,
                wait.scope.plugin_name.as_deref(),
                wait.scope.required_permission,
            )
            .await;
        if !admitted {
            return self.waits.cancel_if_current(wait);
        }
        self.waits.respond_if_current(
            wait,
            WaitEvent {
                raw: event.clone(),
                domain: domain.cloned(),
                locale,
            },
        )
    }

    pub(crate) async fn allowed(
        &self,
        event: &Value,
        plugin: Option<&str>,
        required: Option<nuono_plugin::core::command::PermissionLevel>,
    ) -> bool {
        let user = super::runtime_outbound::runtime_identity(event, "user_id");
        let group = super::runtime_outbound::runtime_identity(event, "group_id");
        let blacklist = self.handler.global_blacklist.read().await;
        let blocked = user.as_ref().is_some_and(|id| blacklist.contains(id))
            || group.as_ref().is_some_and(|id| blacklist.contains(id));
        drop(blacklist);
        *self.handler.is_handle.read().await
            && !blocked
            && matches!(
                self.filter.handle_event(event).await,
                Ok(FilterResponse::Continue)
            )
            && self.host.wait_reply_allowed(plugin, required, event).await
    }
}
