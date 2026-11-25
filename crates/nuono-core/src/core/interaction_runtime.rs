//! 在进入常规运行时管线阶段之前，优先处理已通过签名校验的交互按钮点击事件。
use super::{EventKernel, RuntimeEventContext, RuntimeEventProcessResult};
use crate::core::runtime_stages::PluginHostRuntimeStage;
use nuono_plugin::api::{
    button::ButtonPermission,
    interaction::{InteractionAction, PluginInteraction},
};
use serde_json::Value;

pub(super) enum InteractionDispatch {
    Command(
        String,
        crate::core::plugin_host::interactions::InteractionEpoch,
    ),
    Finished(RuntimeEventProcessResult),
}

impl EventKernel {
    pub(super) async fn try_deliver_interaction_reply(&self, bot_id: i64, event: &Value) -> bool {
        if !self.plugin_host.is_registered_reply_interaction(event) {
            return false;
        }
        let Some(claimed) = self.plugin_host.claim_interaction(event).await else {
            return false;
        };
        let Some(wait) = claimed.wait else {
            return true;
        };
        let domain = crate::core::interaction::message_domain(&claimed.message);
        let locale = crate::localization::resolve_event_or_default(
            &self.locales,
            &claimed.message,
            domain.as_ref(),
        )
        .await;
        self.wait_delivery
            .deliver_registered(bot_id, &wait, &claimed.message, domain.as_ref(), locale)
            .await;
        true
    }
    pub(super) async fn dispatch_runtime_interaction(
        &self,
        ctx: &mut RuntimeEventContext<'_>,
    ) -> InteractionDispatch {
        let denied = || {
            InteractionDispatch::Finished(RuntimeEventProcessResult {
                blocked: true,
                is_message: false,
            })
        };
        let Some(claimed) = self.plugin_host.claim_interaction(&ctx.event).await else {
            return denied();
        };
        let domain = crate::core::interaction::message_domain(&claimed.message);
        let scope = match self
            .plugin_host
            .event_configuration_scope(domain.as_ref())
            .await
        {
            Ok(scope) => scope,
            Err(_) => return denied(),
        };
        nuono_plugin::config::replace_runtime_configurations(scope);
        let required = (claimed.permission == ButtonPermission::Manager)
            .then_some(nuono_plugin::core::command::PermissionLevel::GroupAdmin);
        if !matches!(claimed.action, InteractionAction::Reply { .. })
            && !self
                .wait_delivery
                .allowed(&claimed.message, Some(&claimed.plugin), required)
                .await
        {
            return denied();
        }
        let domain = crate::core::interaction::message_domain(&claimed.message);
        ctx.locale = crate::localization::resolve_event_or_default(
            &self.locales,
            &claimed.message,
            domain.as_ref(),
        )
        .await;
        match claimed.action {
            InteractionAction::Command { command } => {
                if !self
                    .plugin_host
                    .interaction_command_is_current(&claimed.plugin, &command)
                    .await
                {
                    return denied();
                }
                ctx.event = claimed.message;
                ctx.domain_event = domain;
                ctx.unified_event = None;
                ctx.is_message = true;
                InteractionDispatch::Command(claimed.plugin, claimed.epoch)
            }
            InteractionAction::Reply { .. } => {
                let Some(wait) = claimed.wait else {
                    return denied();
                };
                self.wait_delivery
                    .deliver_registered(
                        ctx.bot_id,
                        &wait,
                        &claimed.message,
                        domain.as_ref(),
                        ctx.locale,
                    )
                    .await;
                denied()
            }
            InteractionAction::Callback { callback_id, data } => {
                let id = claimed
                    .message
                    .get("interaction_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let interaction = PluginInteraction {
                    id,
                    callback_id,
                    data,
                };
                ctx.event = claimed.message;
                ctx.domain_event = domain;
                ctx.unified_event = None;
                let result = crate::core::plugin_host::trigger_control::with_message_admission(
                    self.plugin_host.dispatch_interaction(
                        &claimed.plugin,
                        &ctx.event,
                        &interaction,
                        claimed.reservation,
                        ctx.domain_event.as_ref(),
                        ctx.locale,
                    ),
                )
                .await;
                let stage = PluginHostRuntimeStage::new(
                    self.plugin_host.clone(),
                    self.plugin_host.wait_manager.clone(),
                    self.permission_manager.clone(),
                );
                if let Err(error) = stage.apply_response(ctx, result).await {
                    tracing::warn!(plugin = %claimed.plugin, error = %error, "interaction response failed");
                }
                // 单次交互点击只归属于注册该按钮的插件，绝不向其他普通事件插件广播。
                ctx.blocked = true;
                denied()
            }
            _ => denied(),
        }
    }
}
