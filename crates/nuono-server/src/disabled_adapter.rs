//! 未安装任何聊天平台适配器时的空实现占位（此时无外部平台协议编解码，仅沙盒测试会话直接向事件内核提交标准事件）。
use nuono_adapter::{NuonoError, Result};
use nuono_app::platform::*;
#[derive(Debug)]
pub(crate) struct DisabledAdapter;
#[async_trait::async_trait]
impl PlatformAdapter for DisabledAdapter {
    fn platform_name(&self) -> &str {
        "disabled"
    }
    fn platform_version(&self) -> &str {
        "1"
    }
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }
    fn extract_self_id(&self, _: &str) -> Option<i64> {
        None
    }
    fn parse_incoming(&self, _: &str) -> Result<UnifiedEvent> {
        Err(NuonoError::Platform("未安装平台适配器".into()))
    }
    fn build_action(&self, _: &Action) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn build_send_message(&self, _: &MessageType, _: i64, _: &str) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn segments_to_protocol(&self, _: &[UnifiedSegment]) -> serde_json::Value {
        serde_json::Value::Null
    }
}
