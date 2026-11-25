//! 工具执行结果的文本与多模态图片投影，以及历史工具图片的体积裁剪。
use super::super::contracts::{
    AgentToolImage, MAX_TOOL_MEDIA_BYTES, ToolExecution, ToolExecutionStatus,
};
use crate::ai::{ChatMessage, MessageRole};
use serde_json::{Value, json};

impl ToolExecution {
    /// 生成写入工具结果消息的 JSON：无图片时保持传统格式；
    /// 有图片时 JSON 内仅保留图片审计摘要，实际图片数据放入 `ChatMessage.images`。
    pub(crate) fn text_result(&self) -> Value {
        let mut value = json!({
            "tool_name": self.tool_name, "risk_level": self.risk_level,
            "status": self.status, "input": self.input, "output": self.output,
            "elapsed_ms": self.elapsed_ms,
        });
        if !self.images.is_empty() {
            value["images"] = self.image_metadata();
        }
        value
    }
    pub(crate) fn image_metadata(&self) -> Value {
        Value::Array(self.images.iter().map(AgentToolImage::metadata).collect())
    }
}

pub(in crate::agent) fn append_tool_result(
    messages: &mut Vec<ChatMessage>,
    call_id: String,
    execution: &ToolExecution,
) {
    let images = if execution.status == ToolExecutionStatus::Succeeded {
        execution
            .images
            .iter()
            .map(|image| image.source().to_owned())
            .collect()
    } else {
        Vec::new()
    };
    messages.push(
        ChatMessage::tool_result(call_id, execution.text_result().to_string()).with_images(images),
    );
    trim_tool_images(messages);
    crate::agent::input::trim_history_images(messages);
}

/// 独立限制上下文中工具返回图片的总字节数（与用户发送的图片分开统计）；
/// 超限时从最早的工具消息起整批移除图片，同时保留工具文字结果并追加省略说明。
pub(in crate::agent) fn trim_tool_images(messages: &mut [ChatMessage]) {
    let mut bytes = 0usize;
    for message in messages
        .iter_mut()
        .rev()
        .filter(|message| message.role == MessageRole::Tool)
    {
        let size = message.images.iter().map(String::len).sum::<usize>();
        if size > MAX_TOOL_MEDIA_BYTES.saturating_sub(bytes) {
            message.images.clear();
            message
                .content
                .push_str("\n[更早的工具图片已移出本轮视觉上下文；工具文字结果仍保留。]");
        } else {
            bytes += size;
        }
    }
}
