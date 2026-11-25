//! 个人账号私聊会话的审计脱敏：审计记录中仅保留运行期路由与操作元数据，绝不落盘聊天正文。

use nuono_app::agent::{ACCOUNT_CHAT_PLATFORM, ACCOUNT_CHAT_PREFIX};
use serde_json::Value;

/// 对个人账号私聊相关的审计事件做脱敏裁剪，剔除对话内容，仅保留不含正文的运维元数据。
///
/// 在写入和读取审计日志时都会执行该过滤，从而兼容清理旧版本遗留的历史记录。
pub fn diagnostic_event_data(event_type: &str, data: Value) -> Value {
    if !event_type.starts_with("assistant.") && !contains_account_scope(&data) {
        return data;
    }
    let Value::Object(mut fields) = data else {
        return serde_json::json!({"private": true});
    };
    fields.retain(|key, value| {
        matches!(
            key.as_str(),
            "session_id"
                | "run_id"
                | "turn_id"
                | "trace_id"
                | "bot_id"
                | "platform"
                | "message_type"
                | "user_id"
                | "group_id"
                | "channel_id"
                | "message_id"
                | "memory_id"
                | "scope_type"
                | "scope_key"
                | "action"
                | "status"
                | "error_kind"
                | "provider"
                | "model"
                | "elapsed_ms"
                | "timeout_secs"
                | "attempt_index"
                | "total_candidates"
                | "fallback_remaining"
                | "tokens"
                | "rounds"
                | "tool_calls"
                | "deleted"
                | "count"
        ) && !value.is_array()
            && !value.is_object()
    });
    fields.insert("private".into(), Value::Bool(true));
    Value::Object(fields)
}

fn contains_account_scope(data: &Value) -> bool {
    match data {
        Value::String(value) => {
            value == ACCOUNT_CHAT_PLATFORM || value.starts_with(ACCOUNT_CHAT_PREFIX)
        }
        Value::Object(fields) => fields.values().any(contains_account_scope),
        Value::Array(values) => values.iter().any(contains_account_scope),
        _ => false,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/audit/privacy.rs"]
mod tests;
