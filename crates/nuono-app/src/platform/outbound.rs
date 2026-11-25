//! 出站消息段 JSON 编码、提及目标提取与发送回执归一化工具。

use super::UnifiedSegment;
use serde_json::Value;

/// 将 `UnifiedSegment` 列表转换为运行时与适配器共用的 OneBot 格式消息段 JSON 数组。
pub fn compatibility_segments(segments: &[UnifiedSegment]) -> Value {
    Value::Array(
        segments
            .iter()
            .map(|segment| match segment {
                UnifiedSegment::Text { text } => {
                    serde_json::json!({"type":"text","data":{"text":text}})
                }
                UnifiedSegment::Image { url } => {
                    serde_json::json!({"type":"image","data":{"file":url}})
                }
                UnifiedSegment::At { user_id } => {
                    serde_json::json!({"type":"at","data":{"qq":user_id}})
                }
                UnifiedSegment::AtUser { user_id } => {
                    serde_json::json!({"type":"at","data":{"qq":user_id}})
                }
                UnifiedSegment::AtAll => serde_json::json!({"type":"at","data":{"qq":"all"}}),
                UnifiedSegment::Face { id } => serde_json::json!({"type":"face","data":{"id":id}}),
                UnifiedSegment::Reply { message_id } => {
                    serde_json::json!({"type":"reply","data":{"id":message_id}})
                }
                UnifiedSegment::File { url, name } => {
                    serde_json::json!({"type":"file","data":{"file":url,"name":name}})
                }
                UnifiedSegment::Voice { url } => {
                    serde_json::json!({"type":"record","data":{"file":url}})
                }
                UnifiedSegment::Video { url } => {
                    serde_json::json!({"type":"video","data":{"file":url}})
                }
                UnifiedSegment::Custom { type_name, data } => {
                    serde_json::json!({"type":type_name,"data":data})
                }
            })
            .collect(),
    )
}

/// 从兼容格式的 `@` 消息段 data 中提取精确的目标用户 ID。
/// 拒绝浮点数、布尔值和空字符串；数字值必须为 JSON 整数以免发生精度舍入。
pub fn mention_user_id(data: &Value) -> Option<String> {
    let value = data.get("qq").or_else(|| data.get("user_id"))?;
    match value {
        Value::String(id) if !id.trim().is_empty() => Some(id.clone()),
        Value::Number(id) => id
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| id.as_u64().map(|id| id.to_string())),
        _ => None,
    }
}

pub fn normalized_send_result(mut raw_messages: Vec<Value>) -> Value {
    let message_ids = raw_messages
        .iter()
        .filter_map(|raw| {
            raw.get("message_id")
                .or_else(|| raw.get("id"))
                .or_else(|| raw.get("msg_id"))
                .filter(|id| id.is_string() || id.is_number())
                .cloned()
        })
        .collect::<Vec<_>>();
    let raw = if raw_messages.len() == 1 {
        raw_messages.pop().unwrap_or(Value::Null)
    } else {
        Value::Array(raw_messages)
    };
    let message_id = (message_ids.len() == 1).then(|| message_ids[0].clone());
    let mut result = serde_json::json!({
        "message_ids": message_ids,
        "raw": raw,
    });
    if let Some(message_id) = message_id {
        result["message_id"] = message_id;
    }
    result
}
