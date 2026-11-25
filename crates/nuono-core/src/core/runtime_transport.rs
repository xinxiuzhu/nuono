use crate::core::RuntimeActionSink;
pub(crate) use nuono_plugin::core::runtime_transport::{SinkTransport, route_identity};
use serde_json::Value;
/// OneBot 底层发送通道封装；走强类型应用层派发时会在入队前补记出站消息日志。
pub(crate) struct OneBotChannelSink(
    pub tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
);
#[async_trait::async_trait]
impl RuntimeActionSink for OneBotChannelSink {
    async fn send_action(&self, action: Value) -> crate::error::Result<()> {
        self.0
            .send(nuono_app::transport::TransportFrame::Text(
                action.to_string(),
            ))
            .await
            .map_err(|e| crate::error::NuonoError::Connection(e.to_string()))
    }
    async fn call_action(&self, _action: Value) -> crate::error::Result<Value> {
        Err(crate::error::NuonoError::Platform(
            "OneBot channel notify transport cannot collect a receipt".into(),
        ))
    }
}
