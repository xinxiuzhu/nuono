//! 管理面板手动发信与定时任务推送共用的路由解析及富消息投递服务。
use crate::{
    core::runtime_action_router::{RuntimeActionReceipt, RuntimeActionRouter},
    error::{NuonoError, Result},
};
use nuono_app::{
    action::ActionAuthority, managed_messages::ManagedMessage, transport::TransportAction,
};

pub async fn send(
    router: &RuntimeActionRouter,
    message: &ManagedMessage,
    authority: ActionAuthority,
) -> Result<RuntimeActionReceipt> {
    let action = message.action()?;
    let bot = nuono_plugin::core::runtime_transport::route_identity(&message.route)
        .map_err(|error| NuonoError::Validation(error.to_string()))?;
    router
        .call_typed_routed(
            &message.route,
            TransportAction::new(bot, action).with_authority(authority),
        )
        .await
}
