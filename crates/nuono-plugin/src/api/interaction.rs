//! 由宿主签发与调度的按钮交互接口；下发给客户端的交互令牌（token）仅作不透明标识，绝不内嵌可执行的指令或业务数据。
use crate::{
    api::{button::ButtonPermission, routed::RuntimeMessageTarget},
    core::RuntimeBotRoute,
    error::Result,
};
use serde_json::Value;
pub type InteractionPortSource =
    std::sync::Arc<std::sync::RwLock<Option<std::sync::Arc<dyn PluginInteractionPort>>>>;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum InteractionAction {
    Command { command: String },
    Reply { value: String },
    Callback { callback_id: String, data: Value },
}

#[derive(Debug, Clone)]
pub struct InteractionRegistration {
    pub plugin: String,
    pub target: RuntimeMessageTarget,
    pub actor_id: String,
    pub origin_chat_id: Option<String>,
    pub permission: ButtonPermission,
    pub allowed_user_ids: Vec<String>,
    pub action: InteractionAction,
    pub ttl_secs: u32,
}

#[derive(Debug, Clone)]
pub struct PluginInteraction {
    pub id: String,
    pub callback_id: String,
    pub data: Value,
}

#[async_trait::async_trait]
pub trait PluginInteractionPort: Send + Sync {
    async fn register(&self, request: InteractionRegistration) -> Result<String>;
    async fn resolve_command(&self, _plugin: &str, _command: &str) -> Result<String> {
        Err(crate::error::NuonoError::NotImplemented(
            "宿主未提供插件命令解析".into(),
        ))
    }
    async fn message_policy(
        &self,
        _route: &RuntimeBotRoute,
    ) -> Result<nuono_app::message_interactions::MessagePolicy> {
        Ok(Default::default())
    }
}
