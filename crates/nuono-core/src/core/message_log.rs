//! 面向运维日志的聊天消息单行摘要。
//!
//! 群和频道输出有限内容预览；私聊及无法确认场景的发送只输出元数据。
//! 命令排障使用声明命令、执行阶段和回执，不依赖私人消息正文。

use std::sync::Arc;

use async_trait::async_trait;
use nuono_app::transport::TransportFrame;
use nuono_plugin::core::action_log::{self, SendState};
use serde_json::Value;

use crate::config::ConfigManager;
use crate::core::event_kernel::RuntimeActionSink;
use crate::core::transport::{ActionTransport, ConnectionId, ProtocolDescriptor, TransportAction};
use crate::domain::{
    ActionError, ActionReceipt, BotIdentity, CapabilitySet, ConversationRef, DeliveryTarget,
    OutboundAction,
};
use crate::error::Result;

const DEFAULT_PREVIEW_CHARS: usize = 400;
const PRIVATE_CONTENT: &str = "[私人消息正文已隐藏]";

fn diagnostic_preview(scene: &str, raw: &str, max_chars: usize) -> (String, bool) {
    if !matches!(scene, "group" | "channel") {
        return (PRIVATE_CONTENT.into(), false);
    }
    let (preview, truncated) = truncate_preview(raw, max_chars);
    if preview.is_empty() {
        ("[空消息]".into(), truncated)
    } else {
        (preview, truncated)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageLogSummary {
    pub line: String,
    pub platform: String,
    pub scene: String,
    pub conversation_id: String,
    pub sender_id: Option<String>,
    pub message_id: Option<String>,
    pub content_length: usize,
    pub truncated: bool,
    pub request_id: Option<String>,
}

pub(crate) fn event_bot_id(event: &Value, fallback: i64) -> String {
    identity(event.get("self_id")).unwrap_or_else(|| fallback.to_string())
}

pub(crate) fn inbound_summary(event: &Value, max_chars: usize) -> Option<MessageLogSummary> {
    if event.get("post_type").and_then(Value::as_str) != Some("message") {
        return None;
    }

    let platform = normalize_platform(
        event
            .get("platform")
            .and_then(Value::as_str)
            .unwrap_or("onebot_v11"),
    );
    let message_type = event
        .get("message_type")
        .and_then(Value::as_str)
        .unwrap_or("private");
    let (scene, scene_label, conversation_id) = scene_from_event(event, message_type);
    let sender_id = event
        .get("sender")
        .and_then(|sender| identity(sender.get("user_id")))
        .or_else(|| identity(event.get("openid")))
        .or_else(|| identity(event.get("user_id")))
        .unwrap_or_else(|| "未知用户".to_string());
    let sender_name = ["card", "nickname"]
        .iter()
        .find_map(|key| {
            event
                .get("sender")
                .and_then(|sender| sender.get(*key))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(sanitize_text)
        })
        .unwrap_or_else(|| sender_id.clone());
    let sender = if sender_name == sender_id {
        sender_id.clone()
    } else {
        format!("{sender_name}({sender_id})")
    };

    let raw_preview = message_preview(
        event.get("message").unwrap_or(&Value::Null),
        event.get("raw_message").and_then(Value::as_str),
    );
    let content_length = raw_preview.chars().count();
    let (preview, truncated) = diagnostic_preview(&scene, &raw_preview, max_chars);
    let line =
        format!("[接收][{platform}] {scene_label}({conversation_id}) <= {sender}: {preview}");

    Some(MessageLogSummary {
        line,
        platform,
        scene,
        conversation_id,
        sender_id: Some(sender_id),
        message_id: identity(event.get("message_id")),
        content_length,
        truncated,
        request_id: None,
    })
}

pub(crate) fn outbound_summary(
    platform: &str,
    action: &Value,
    max_chars: usize,
) -> Option<MessageLogSummary> {
    let action_name = action
        .get("action")
        .or_else(|| action.get("method"))
        .and_then(Value::as_str)?;
    let platform = normalize_platform(platform);
    let standard_message_action = matches!(
        action_name,
        "send_msg" | "send_private_msg" | "send_group_msg" | "send_guild_channel_msg"
    );
    let native_message_action = match platform.as_str() {
        "qq_official" => action_name == "send_message",
        "feishu" => matches!(action_name, "reply_message" | "replyMessage"),
        "telegram" => matches!(
            action_name,
            "sendMessage" | "sendPhoto" | "sendDocument" | "sendVoice" | "sendVideo"
        ),
        "discord" => action_name == "sendMessage",
        _ => false,
    };
    if !standard_message_action && !native_message_action {
        return None;
    }

    let params = action.get("params").unwrap_or(action);
    let message_type = match action_name {
        "send_private_msg" => "private",
        "send_group_msg" => "group",
        "send_guild_channel_msg" => "channel",
        "reply_message" | "replyMessage" => "reply",
        _ => params
            .get("message_type")
            .or_else(|| params.get("qq_target_type"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                if params.get("group_id").is_some() || params.get("group_openid").is_some() {
                    "group"
                } else if params.get("channel_id").is_some() {
                    "channel"
                } else if params.get("chat_id").is_some() {
                    "conversation"
                } else {
                    "private"
                }
            }),
    };
    let (scene, scene_label, conversation_id) = scene_from_event(params, message_type);
    let message = params
        .get("message")
        .or_else(|| params.get("content"))
        .or_else(|| params.get("text"))
        .or_else(|| action.get("message"))
        .or_else(|| action.get("content"))
        .or_else(|| action.get("text"));
    let mut raw_preview = message_preview(message.unwrap_or(&Value::Null), None);
    if raw_preview.is_empty()
        && let Some(media) = native_media_label(action_name)
    {
        raw_preview.push_str(media);
    }
    let content_length = raw_preview.chars().count();
    let (preview, truncated) = diagnostic_preview(&scene, &raw_preview, max_chars);
    let line = format!("[发送][{platform}] {scene_label}({conversation_id}) => {preview}");

    Some(MessageLogSummary {
        line,
        platform,
        scene,
        conversation_id,
        sender_id: None,
        message_id: identity(params.get("message_id")),
        content_length,
        truncated,
        request_id: identity(action.get("echo")),
    })
}

/// 超过此字节上限的出站帧将直接通过字节扫描生成摘要，不再做完整 JSON 反序列化。
///
/// 内嵌 Base64 媒体的出站帧体积远超普通聊天消息；若将其完整解析为 `Value`，会为了打印一行 `[视频]` 而分配巨量 JSON 节点。
pub(crate) const OUTBOUND_PARSE_LIMIT: usize = 64 * 1024;

pub(crate) fn outbound_action_from_ws_message(message: &TransportFrame) -> Option<Value> {
    let TransportFrame::Text(text) = message else {
        return None;
    };
    if text.len() > OUTBOUND_PARSE_LIMIT {
        return None;
    }
    serde_json::from_str(text.as_str()).ok()
}

/// 大帧的替代摘要：只扫描一次字节，按消息段类型给出与解析后一致的标签。
///
/// 触发条件与 [`outbound_action_from_ws_message`] 互斥，两者放在一起保证大媒体帧
/// 在日志路径上永远不会被完整反序列化。
pub(crate) fn oversized_outbound_summary(
    platform: &str,
    frame: &str,
    max_chars: usize,
) -> Option<MessageLogSummary> {
    let action_name = scan_first_string_field(frame, &["action"])?;
    if !matches!(
        action_name.as_str(),
        "send_msg" | "send_private_msg" | "send_group_msg" | "send_guild_channel_msg"
    ) {
        return None;
    }
    // 与 `outbound_summary` 保持一致的会话场景推断：按路由发送时使用的 `send_msg` 会在 params 内携带 message_type 或 group_id。
    let (scene, scene_label) = match action_name.as_str() {
        "send_group_msg" => ("group", "群"),
        "send_guild_channel_msg" => ("channel", "频道"),
        "send_private_msg" => ("private", "私聊"),
        _ => {
            let has_group = scan_first_string_field(frame, &["group_id", "group_openid"]).is_some();
            let has_channel = scan_first_string_field(frame, &["channel_id"]).is_some();
            if has_group {
                ("group", "群")
            } else if has_channel {
                ("channel", "频道")
            } else {
                ("private", "私聊")
            }
        }
    };
    let target_keys: &[&str] = match scene {
        "group" => &["group_id", "group_openid", "chat_id"],
        "channel" => &["channel_id", "chat_id"],
        _ => &["user_id", "openid", "chat_id"],
    };
    let conversation_id =
        scan_first_string_field(frame, target_keys).unwrap_or_else(|| "未知目标".to_string());
    let preview = if matches!(scene, "group" | "channel") {
        scan_segment_kind(frame).unwrap_or_else(|| "[消息]".to_string())
    } else {
        PRIVATE_CONTENT.into()
    };
    let platform = normalize_platform(platform);
    let line = format!("[发送][{platform}] {scene_label}({conversation_id}) => {preview}");
    let (line, truncated) = truncate_preview(&line, max_chars.max(DEFAULT_PREVIEW_CHARS));
    Some(MessageLogSummary {
        line,
        platform,
        scene: scene.to_string(),
        conversation_id,
        sender_id: None,
        message_id: scan_first_string_field(frame, &["message_id"]),
        content_length: preview.chars().count(),
        truncated,
        request_id: scan_first_string_field(frame, &["echo"]),
    })
}

/// 找出第一个 `"key":"value"` 形式出现的目标键；只做字节扫描，不建 AST。
fn scan_first_string_field(frame: &str, keys: &[&str]) -> Option<String> {
    for key in keys {
        let pattern = format!("\"{key}\":\"");
        if let Some(start) = frame.find(&pattern) {
            let rest = &frame[start + pattern.len()..];
            let end = rest.find('"')?;
            let value = &rest[..end];
            let value = sanitize_inline(value);
            if !value.is_empty() {
                return Some(value);
            }
        }
        // 数值形式的 ID（如 `"group_id":123`）采用相同方式扫描连续数字字符。
        let pattern = format!("\"{key}\":");
        if let Some(start) = frame.find(&pattern) {
            let rest = &frame[start + pattern.len()..];
            let digits: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_digit() || *ch == '-')
                .collect();
            if !digits.is_empty() {
                return Some(digits);
            }
        }
    }
    None
}

/// 段类型按出现顺序优先取媒体标签；媒体段通常只有一个，扫描成本是线性的。
fn scan_segment_kind(frame: &str) -> Option<String> {
    for (needle, label) in [
        ("\"type\":\"video\"", "[视频]"),
        ("\"type\":\"record\"", "[语音]"),
        ("\"type\":\"file\"", "[文件]"),
        ("\"type\":\"image\"", "[图片]"),
        ("\"type\":\"forward\"", "[转发消息]"),
        ("\"type\":\"node\"", "[转发消息]"),
    ] {
        if frame.contains(needle) {
            return Some(label.to_string());
        }
    }
    // 文本：取第一个 text 字段，脱敏后截断。
    let pattern = "\"text\":\"";
    let start = frame.find(pattern)? + pattern.len();
    let rest = &frame[start..];
    let end = rest.find('"').unwrap_or(rest.len());
    let text = sanitize_text(&rest[..end]);
    let (text, _) = truncate_preview(&text, DEFAULT_PREVIEW_CHARS);
    (!text.trim().is_empty()).then_some(text)
}

/// 供非日志摘要场景（如 Matcher 命中提示）复用的安全预览：
/// URL / Base64 / 本地路径脱敏后按 `max_chars` 截断。
#[cfg(test)]
fn safe_content_preview(text: &str, max_chars: usize) -> String {
    let sanitized = sanitize_text(text);
    truncate_preview(&sanitized, max_chars).0
}

pub(crate) fn log_receive(bot_id: &str, summary: &MessageLogSummary) {
    tracing::info!(
        target: "message_flow",
        bot_id = %bot_id,
        direction = "receive",
        platform = %summary.platform,
        scene = %summary.scene,
        conversation_id = %summary.conversation_id,
        sender_id = summary.sender_id.as_deref().unwrap_or(""),
        message_id = summary.message_id.as_deref().unwrap_or(""),
        content_length = summary.content_length,
        truncated = summary.truncated,
        "{}",
        summary.line
    );
}

pub(crate) fn log_send(bot_id: &str, summary: &MessageLogSummary) {
    log_send_outcome(bot_id, summary, SendState::Submitted, None, None);
}

pub(crate) fn log_send_outcome(
    bot_id: &str,
    summary: &MessageLogSummary,
    state: SendState,
    plugin: Option<&str>,
    error: Option<&str>,
) {
    macro_rules! emit {
        ($level:expr) => { tracing::event!(
        target: "message_flow",
        $level,
        bot_id = %bot_id,
        plugin,
        send_state = state.as_str(),
        request_id = summary.request_id.as_deref(),
        error,
        direction = "send",
        platform = %summary.platform,
        scene = %summary.scene,
        conversation_id = %summary.conversation_id,
        message_id = summary.message_id.as_deref().unwrap_or(""),
        content_length = summary.content_length,
        truncated = summary.truncated,
        "{}",
        summary.line
        ) };
    }
    match state {
        SendState::Failed | SendState::Timeout | SendState::Unconfirmed | SendState::Cancelled => {
            emit!(tracing::Level::WARN)
        }
        _ => emit!(tracing::Level::INFO),
    }
}

pub(crate) fn typed_outbound_summary(
    bot: &BotIdentity,
    action: &OutboundAction,
    max_chars: usize,
) -> Option<MessageLogSummary> {
    let OutboundAction::SendMessage {
        target,
        segments,
        reply_to: _,
    } = action
    else {
        return None;
    };
    let (action_name, mut params) = match target {
        DeliveryTarget::Actor { actor } => (
            "send_private_msg",
            serde_json::json!({"user_id": actor.value().as_str()}),
        ),
        DeliveryTarget::Conversation { conversation } => match conversation {
            ConversationRef::Direct {
                participant,
                conversation,
            } => (
                "send_private_msg",
                serde_json::json!({
                    "user_id": conversation.as_ref().unwrap_or(participant).value().as_str()
                }),
            ),
            ConversationRef::Group { group } => (
                "send_group_msg",
                serde_json::json!({"group_id": group.value().as_str()}),
            ),
            ConversationRef::Channel { channel, .. } => (
                "send_guild_channel_msg",
                serde_json::json!({"channel_id": channel.value().as_str()}),
            ),
            _ => return None,
        },
        DeliveryTarget::Channel { channel, .. } => (
            "send_guild_channel_msg",
            serde_json::json!({"channel_id": channel.value().as_str()}),
        ),
        _ => return None,
    };
    params["message"] = serde_json::to_value(segments).ok()?;
    outbound_summary(
        bot.account_id.platform().as_str(),
        &serde_json::json!({"action": action_name, "params": params}),
        max_chars,
    )
}

pub(crate) struct LoggingRuntimeActionSink {
    inner: Arc<dyn RuntimeActionSink>,
    config: Arc<ConfigManager>,
    platform: String,
    bot_id: String,
}

impl LoggingRuntimeActionSink {
    pub(crate) fn new(
        inner: Arc<dyn RuntimeActionSink>,
        config: Arc<ConfigManager>,
        platform: impl Into<String>,
        bot_id: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            config,
            platform: platform.into(),
            bot_id: bot_id.into(),
        }
    }

    /// 构造单行出站日志摘要；若消息日志已关闭则返回 `None`。
    ///
    /// 调用方在将动作所有权移交给发送通道前先传引用生成摘要，避免为了打一行日志而克隆大体积媒体载荷。
    async fn outbound_summary_for(&self, action: &Value) -> Option<MessageLogSummary> {
        let config = self.config.snapshot().await;
        if !config.log.show_send || normalize_platform(&self.platform) == "onebot_v11" {
            return None;
        }
        let mut summary =
            outbound_summary(&self.platform, action, config.log.message_preview_chars)?;
        summary
            .request_id
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
        Some(summary)
    }
}

#[async_trait]
impl RuntimeActionSink for LoggingRuntimeActionSink {
    async fn send_action(&self, action: Value) -> Result<()> {
        // 先借用动作生成日志摘要，再将其所有权移入底层发送通道。
        let summary = self.outbound_summary_for(&action).await;
        let result = self.inner.send_action(action).await;
        if let Some(summary) = summary {
            let error = result.as_ref().err().map(ToString::to_string);
            log_send_outcome(
                &self.bot_id,
                &summary,
                if result.is_ok() {
                    SendState::Queued
                } else {
                    SendState::Failed
                },
                action_log::current_plugin().as_deref(),
                error.as_deref(),
            );
        }
        result
    }

    async fn call_action(&self, action: Value) -> Result<Value> {
        let summary = self.outbound_summary_for(&action).await;
        let response = self.inner.call_action(action).await;
        if let Some(summary) = summary {
            let error = response.as_ref().err().map(ToString::to_string);
            let state = match &response {
                Ok(value)
                    if value.get("message_id").is_some_and(|id| !id.is_null())
                        || value
                            .get("message_ids")
                            .and_then(Value::as_array)
                            .is_some_and(|ids| !ids.is_empty()) =>
                {
                    SendState::Confirmed
                }
                Ok(_) => SendState::Submitted,
                Err(_) => SendState::Failed,
            };
            log_send_outcome(
                &self.bot_id,
                &summary,
                state,
                action_log::current_plugin().as_deref(),
                error.as_deref(),
            );
        }
        response
    }
}

pub(crate) struct LoggingActionTransport {
    inner: Arc<dyn ActionTransport>,
    config: Arc<ConfigManager>,
}

impl LoggingActionTransport {
    pub(crate) fn new(inner: Arc<dyn ActionTransport>, config: Arc<ConfigManager>) -> Self {
        Self { inner, config }
    }

    async fn summary_for(&self, action: &TransportAction) -> Option<MessageLogSummary> {
        let config = self.config.snapshot().await;
        if !config.log.show_send {
            return None;
        }
        let mut summary = typed_outbound_summary(
            &action.bot,
            &action.action,
            config.log.message_preview_chars,
        )?;
        summary.request_id = Some(uuid::Uuid::new_v4().to_string());
        Some(summary)
    }
}

#[async_trait]
impl ActionTransport for LoggingActionTransport {
    fn connection_id(&self) -> &ConnectionId {
        self.inner.connection_id()
    }

    fn protocol(&self) -> &ProtocolDescriptor {
        self.inner.protocol()
    }

    fn capabilities(&self) -> CapabilitySet {
        self.inner.capabilities()
    }

    async fn notify(&self, action: TransportAction) -> std::result::Result<(), ActionError> {
        let summary = self.summary_for(&action).await;
        let bot_id = action.bot.account_id.value().to_string();
        let plugin = action_plugin(&action);
        let result = self.inner.notify(action).await;
        if let Some(summary) = summary {
            log_transport_result(
                &bot_id,
                &summary,
                plugin.as_deref(),
                result.as_ref().map(|_| SendState::Queued),
            );
        }
        result
    }

    async fn call(
        &self,
        action: TransportAction,
    ) -> std::result::Result<ActionReceipt, ActionError> {
        let summary = self.summary_for(&action).await;
        let bot_id = action.bot.account_id.value().to_string();
        let plugin = action_plugin(&action);
        let result = self.inner.call(action).await;
        if let Some(mut summary) = summary {
            if let Ok(receipt) = &result {
                summary.request_id = Some(receipt.correlation_id.to_string());
            }
            log_transport_result(
                &bot_id,
                &summary,
                plugin.as_deref(),
                result.as_ref().map(|receipt| match receipt.status {
                    crate::domain::ActionReceiptStatus::Completed => SendState::Confirmed,
                    crate::domain::ActionReceiptStatus::Accepted | _ => SendState::Submitted,
                }),
            );
        }
        result
    }

    async fn call_raw(
        &self,
        action: &str,
        params: Value,
        bot: Option<&BotIdentity>,
    ) -> std::result::Result<Value, ActionError> {
        let completed = if action == "text_stream"
            && params["protocol"] == 1
            && params["operation"] == "complete"
        {
            bot.and_then(|bot| {
                let target = serde_json::from_value(params["target"].clone()).ok()?;
                let content = params["content"].as_str()?;
                Some(TransportAction::new(
                    bot.clone(),
                    nuono_domain::OutboundAction::SendMessage {
                        target,
                        segments: vec![nuono_domain::MessageSegment::Text {
                            text: content.into(),
                        }],
                        reply_to: None,
                    },
                ))
            })
        } else {
            None
        };
        let result = self.inner.call_raw(action, params, bot).await;
        if let Some(completed) = completed
            && let Some(summary) = self.summary_for(&completed).await
        {
            log_transport_result(
                completed.bot.account_id.value().as_str(),
                &summary,
                action_log::current_plugin().as_deref(),
                result.as_ref().map(|_| SendState::Submitted),
            );
        }
        result
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}

fn action_plugin(action: &TransportAction) -> Option<String> {
    match &action.authority {
        nuono_app::action::ActionAuthority::Plugin { name } => Some(name.clone()),
        _ => action_log::current_plugin(),
    }
}

fn log_transport_result(
    bot_id: &str,
    summary: &MessageLogSummary,
    plugin: Option<&str>,
    result: std::result::Result<SendState, &ActionError>,
) {
    match result {
        Ok(state) => log_send_outcome(bot_id, summary, state, plugin, None),
        Err(error) => log_send_outcome(
            bot_id,
            summary,
            if error.kind == crate::domain::ActionErrorKind::Timeout {
                SendState::Timeout
            } else {
                SendState::Failed
            },
            plugin,
            Some(&error.to_string()),
        ),
    }
}

fn normalize_platform(platform: &str) -> String {
    let normalized = platform
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_");
    match normalized.as_str() {
        "onebot" | "onebot_v11" => "onebot_v11".to_string(),
        "qq" | "qq_official" => "qq_official".to_string(),
        "" => "unknown".to_string(),
        _ => sanitize_inline(&normalized),
    }
}

fn native_media_label(action_name: &str) -> Option<&'static str> {
    match action_name {
        "sendPhoto" => Some("[图片]"),
        "sendDocument" => Some("[文件]"),
        "sendVoice" => Some("[语音]"),
        "sendVideo" => Some("[视频]"),
        _ => None,
    }
}

fn scene_from_event(event: &Value, message_type: &str) -> (String, &'static str, String) {
    match message_type {
        "group" => (
            "group".to_string(),
            "群",
            first_identity(
                event,
                &["group_openid", "group_id", "chat_id", "channel_id"],
            ),
        ),
        "channel" => (
            "channel".to_string(),
            "频道",
            first_identity(event, &["channel_id", "chat_id", "group_id"]),
        ),
        "conversation" => (
            "conversation".to_string(),
            "会话",
            first_identity(event, &["chat_id", "channel_id", "group_id", "user_id"]),
        ),
        "reply" => (
            "reply".to_string(),
            "消息",
            first_identity(event, &["message_id"]),
        ),
        _ => (
            "private".to_string(),
            "私聊",
            first_identity(event, &["chat_id", "channel_id", "openid", "user_id"]),
        ),
    }
}

fn first_identity(event: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| identity(event.get(*key)))
        .unwrap_or_else(|| "未知目标".to_string())
}

fn identity(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| sanitize_inline(value))
        }
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn message_preview(message: &Value, raw_fallback: Option<&str>) -> String {
    let preview = match message {
        Value::String(text) => sanitize_text(text),
        Value::Array(segments) => segments
            .iter()
            .map(segment_preview)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };
    if preview.is_empty() {
        raw_fallback.map(sanitize_text).unwrap_or_default()
    } else {
        preview
    }
}

fn segment_preview(segment: &Value) -> String {
    let kind = segment
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let data = segment.get("data").unwrap_or(&Value::Null);
    match kind {
        "text" => data
            .get("text")
            .and_then(Value::as_str)
            .map(sanitize_text)
            .unwrap_or_default(),
        "image" => "[图片]".to_string(),
        "record" | "voice" | "audio" => "[语音]".to_string(),
        "video" => "[视频]".to_string(),
        "at" => match identity(data.get("qq").or_else(|| data.get("user_id"))) {
            Some(value) if value == "all" => "@全体成员".to_string(),
            Some(value) => format!("@{value}"),
            None => "@用户".to_string(),
        },
        "reply" => identity(data.get("id").or_else(|| data.get("message_id")))
            .map(|id| format!("[回复:{id}]"))
            .unwrap_or_else(|| "[回复]".to_string()),
        "file" => data
            .get("name")
            .and_then(Value::as_str)
            .and_then(safe_file_name)
            .map(|name| format!("[文件:{name}]"))
            .unwrap_or_else(|| "[文件]".to_string()),
        "face" => identity(data.get("id"))
            .map(|id| format!("[表情:{id}]"))
            .unwrap_or_else(|| "[表情]".to_string()),
        "forward" | "node" => "[转发消息]".to_string(),
        "json" => "[JSON消息]".to_string(),
        "xml" => "[XML消息]".to_string(),
        "markdown" => data
            .get("content")
            .and_then(Value::as_str)
            .map(sanitize_text)
            .unwrap_or_else(|| "[Markdown]".to_string()),
        _ => safe_segment_kind(kind)
            .map(|kind| format!("[消息段:{kind}]"))
            .unwrap_or_else(|| "[未知消息段]".to_string()),
    }
}

fn safe_segment_kind(value: &str) -> Option<String> {
    let value = sanitize_inline(value);
    (value.len() <= 32
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then_some(value)
}

fn safe_file_name(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.contains("://") || lower.contains("base64") || lower.starts_with("data:") {
        return None;
    }
    let name = value
        .rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let name = sanitize_inline(&crate::telemetry::redact_text(name));
    let (name, _) = truncate_preview(&name, 80);
    (!name.is_empty()).then_some(name)
}

fn sanitize_text(value: &str) -> String {
    let value = crate::telemetry::redact_text(&strip_ansi(value));
    let mut result = String::with_capacity(value.len());
    let mut token = String::new();

    let flush = |token: &mut String, result: &mut String| {
        if token.is_empty() {
            return;
        }
        let lower = token.to_ascii_lowercase();
        if lower.contains("http://") || lower.contains("https://") {
            result.push_str("[链接]");
        } else if lower.contains("base64://") || lower.contains("data:") || looks_like_base64(token)
        {
            result.push_str("[数据]");
        } else if looks_like_local_path(token) {
            result.push_str("[路径]");
        } else {
            result.push_str(&sanitize_inline(token));
        }
        token.clear();
    };

    for ch in value.chars() {
        if ch.is_whitespace() {
            flush(&mut token, &mut result);
            match ch {
                '\n' => result.push_str("\\n"),
                '\r' => result.push_str("\\r"),
                '\t' => result.push_str("\\t"),
                _ => result.push(ch),
            }
        } else {
            token.push(ch);
        }
    }
    flush(&mut token, &mut result);
    result
}

fn looks_like_local_path(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    let known_absolute_root = [
        "/bin", "/dev", "/etc", "/home", "/opt", "/private", "/proc", "/root", "/run", "/sbin",
        "/srv", "/sys", "/tmp", "/usr", "/users", "/var", "/volumes",
    ]
    .iter()
    .any(|root| lower == *root || lower.starts_with(&format!("{root}/")));
    let relative_path = token.starts_with("./")
        || token.starts_with("../")
        || (!token.starts_with('/') && token.contains('/'));

    lower.starts_with("file://")
        || token.starts_with("~/")
        || token.starts_with("\\\\")
        || known_absolute_root
        || (token.starts_with('/') && token[1..].contains('/'))
        || relative_path
        || (token.len() >= 3
            && token.as_bytes()[1] == b':'
            && matches!(token.as_bytes()[2], b'\\' | b'/'))
}

fn looks_like_base64(token: &str) -> bool {
    let trimmed = token.trim_end_matches('=');
    let padding = token.len() - trimmed.len();
    let standard_alphabet = trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'));
    let url_safe_alphabet = trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    let encoded_shape = padding <= 2
        && trimmed.len() >= 24
        && (standard_alphabet || url_safe_alphabet)
        && token.len() % 4 != 1;
    encoded_shape
        && (padding > 0
            || token.len() >= 48
            || token.chars().any(|ch| matches!(ch, '+' | '/' | '-' | '_')))
}

fn sanitize_inline(value: &str) -> String {
    strip_ansi(value)
        .chars()
        .filter_map(|ch| match ch {
            '\n' => Some("\\n".to_string()),
            '\r' => Some("\\r".to_string()),
            '\t' => Some("\\t".to_string()),
            ch if ch.is_control() => None,
            ch => Some(ch.to_string()),
        })
        .collect()
}

fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            output.push(ch);
            continue;
        }

        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.next_if_eq(&'\\').is_some() {
                        break;
                    }
                }
            }
            Some(_) | None => {}
        }
    }
    output
}

fn truncate_preview(value: &str, max_chars: usize) -> (String, bool) {
    let max_chars = if max_chars == 0 {
        DEFAULT_PREVIEW_CHARS
    } else {
        max_chars
    };
    let length = value.chars().count();
    if length <= max_chars {
        return (value.to_string(), false);
    }
    let omitted = length - max_chars;
    (
        format!(
            "{}... (+{} 字符)",
            value.chars().take(max_chars).collect::<String>(),
            omitted
        ),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};

    #[test]
    fn large_frames_are_never_parsed_and_keep_the_message_shape() {
        let file = format!("base64://{}", "A".repeat(OUTBOUND_PARSE_LIMIT * 2));
        let frame = json!({
            "action": "send_msg",
            "echo": "outbound-video-request",
            "params": {
                "group_id": "100001",
                "message": [{"type": "video", "data": {"file": file}}]
            }
        })
        .to_string();
        assert!(frame.len() > OUTBOUND_PARSE_LIMIT);
        assert!(
            outbound_action_from_ws_message(&TransportFrame::Text(frame.clone())).is_none(),
            "oversized frames must not build a JSON tree"
        );

        let summary = oversized_outbound_summary("OneBot v11", &frame, 400).unwrap();
        assert_eq!(summary.scene, "group");
        assert_eq!(summary.conversation_id, "100001");
        assert_eq!(summary.line, "[发送][onebot_v11] 群(100001) => [视频]");
        assert_eq!(
            summary.request_id.as_deref(),
            Some("outbound-video-request")
        );
        assert_eq!(summary.platform, "onebot_v11");
    }

    #[test]
    fn large_frame_summaries_cover_text_and_private_targets() {
        let text = "长文本".repeat(OUTBOUND_PARSE_LIMIT);
        let frame = json!({
            "action": "send_private_msg",
            "params": {"user_id": 42, "message": [{"type": "text", "data": {"text": text}}]}
        })
        .to_string();
        let summary = oversized_outbound_summary("onebot_v11", &frame, 20).unwrap();
        assert_eq!(summary.scene, "private");
        assert_eq!(summary.conversation_id, "42");
        assert!(!summary.line.contains("长文本"));
        assert!(summary.line.contains(PRIVATE_CONTENT));
    }

    #[test]
    fn large_frames_for_non_message_actions_produce_no_summary() {
        let frame =
            json!({"action":"get_group_list","params":{"pad":"A".repeat(OUTBOUND_PARSE_LIMIT)}})
                .to_string();
        assert!(oversized_outbound_summary("onebot_v11", &frame, 400).is_none());
    }

    struct RecordingSink {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait]
    impl RuntimeActionSink for RecordingSink {
        async fn send_action(&self, _action: Value) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(crate::error::NuonoError::MessageSend(
                    "expected failure".to_string(),
                ));
            }
            Ok(())
        }

        async fn call_action(&self, _action: Value) -> Result<Value> {
            unreachable!("delivery boundary test uses send_action")
        }
    }

    struct MessageFlowCounter(Arc<AtomicUsize>);

    impl<S> Layer<S> for MessageFlowCounter
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            if event.metadata().target() == "message_flow" {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[tokio::test]
    async fn runtime_sink_reports_success_and_failure_without_retry() {
        let root = std::env::temp_dir().join(format!("nuono-message-log-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let config = Arc::new(ConfigManager::for_test(
            crate::config::NuonoConfig::default(),
            root.join("runtime.json"),
        ));
        let log_count = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry().with(MessageFlowCounter(log_count.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);
        let action = json!({
            "action": "send_group_msg",
            "params": {"group_id": 100, "message": "hello"}
        });

        let success_calls = Arc::new(AtomicUsize::new(0));
        let success = LoggingRuntimeActionSink::new(
            Arc::new(RecordingSink {
                calls: success_calls.clone(),
                fail: false,
            }),
            config.clone(),
            "telegram",
            "42",
        );
        success.send_action(action.clone()).await.unwrap();
        assert_eq!(success_calls.load(Ordering::SeqCst), 1);
        assert_eq!(log_count.load(Ordering::SeqCst), 1);

        let failure_calls = Arc::new(AtomicUsize::new(0));
        let failure = LoggingRuntimeActionSink::new(
            Arc::new(RecordingSink {
                calls: failure_calls.clone(),
                fail: true,
            }),
            config,
            "telegram",
            "42",
        );
        assert!(failure.send_action(action).await.is_err());
        assert_eq!(failure_calls.load(Ordering::SeqCst), 1);
        assert_eq!(log_count.load(Ordering::SeqCst), 2);

        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[test]
    fn safe_preview_preserves_adjacent_chinese_characters() {
        assert_eq!(
            safe_content_preview("ww卡片 参数错误", 400),
            "ww卡片 参数错误"
        );
    }

    #[test]
    fn group_receive_is_single_line_and_prefers_card() {
        let event = json!({
            "post_type": "message",
            "platform": "onebot_v11",
            "message_type": "group",
            "self_id": 42,
            "group_id": 100,
            "user_id": 7,
            "message_id": 9,
            "sender": {"user_id": 7, "card": "群名片", "nickname": "昵称"},
            "message": [{"type": "text", "data": {"text": "第一行\n第二行"}}]
        });

        let summary = inbound_summary(&event, 400).unwrap();
        assert_eq!(
            summary.line,
            "[接收][onebot_v11] 群(100) <= 群名片(7): 第一行\\n第二行"
        );
        assert!(!summary.line.contains('\n'));
    }

    #[test]
    fn string_identities_are_preserved() {
        let event = json!({
            "post_type": "message",
            "platform": "discord",
            "message_type": "channel",
            "self_id": "bot-open-id",
            "channel_id": "18446744073709551615",
            "user_id": "user-open-id",
            "sender": {"user_id": "user-open-id"},
            "raw_message": "hello"
        });

        let summary = inbound_summary(&event, 400).unwrap();
        assert_eq!(summary.conversation_id, "18446744073709551615");
        assert!(summary.line.contains("user-open-id"));
    }

    #[test]
    fn private_scene_prefers_real_conversation_identity() {
        let event = json!({
            "post_type": "message",
            "platform": "discord",
            "message_type": "private",
            "channel_id": "dm-channel",
            "user_id": "author",
            "sender": {"user_id": "author"},
            "raw_message": "hello"
        });

        let summary = inbound_summary(&event, 400).unwrap();
        assert_eq!(summary.conversation_id, "dm-channel");
        assert_eq!(
            summary.line,
            "[接收][discord] 私聊(dm-channel) <= author: [私人消息正文已隐藏]"
        );
    }

    #[test]
    fn media_resources_and_unknown_data_are_not_logged() {
        let event = json!({
            "post_type": "message",
            "message_type": "group",
            "group_id": 42,
            "self_id": 42,
            "user_id": 7,
            "sender": {"user_id": 7},
            "message": [
                {"type": "image", "data": {"file": "base64://secret", "url": "https://example.test/?token=secret"}},
                {"type": "file", "data": {"file": "/Users/private/report.pdf", "name": "report.pdf"}},
                {"type": "mystery", "data": {"token": "secret"}},
                {"type": "https://malicious.test", "data": {"token": "secret"}},
                {"type": "file", "data": {"name": "https://malicious.test/report.pdf"}}
            ]
        });

        let summary = inbound_summary(&event, 400).unwrap();
        assert!(
            summary
                .line
                .contains("[图片][文件:report.pdf][消息段:mystery][未知消息段][文件]")
        );
        for secret in ["base64", "https://", "/Users", "token", "secret"] {
            assert!(!summary.line.contains(secret));
        }
    }

    #[test]
    fn outgoing_classifier_ignores_non_message_actions() {
        assert!(outbound_summary("onebot_v11", &json!({"action": "delete_msg"}), 400).is_none());
        assert!(
            outbound_summary("onebot_v11", &json!({"action": "get_group_list"}), 400).is_none()
        );
    }

    #[test]
    fn outgoing_group_message_includes_target_and_safe_segments() {
        let action = json!({
            "action": "send_group_msg",
            "params": {
                "group_id": 100,
                "message": [
                    {"type": "text", "data": {"text": "结果"}},
                    {"type": "at", "data": {"qq": "all"}},
                    {"type": "image", "data": {"file": "https://secret.test/image.png"}}
                ]
            }
        });

        let summary = outbound_summary("onebot_v11", &action, 400).unwrap();
        assert_eq!(
            summary.line,
            "[发送][onebot_v11] 群(100) => 结果@全体成员[图片]"
        );
        assert!(!summary.line.contains("https://"));
    }

    #[test]
    fn native_outbound_actions_only_preview_confirmed_group_content() {
        let telegram = json!({
            "method": "sendPhoto",
            "params": {
                "chat_id": -100123,
                "photo": "https://secret.test/photo.jpg"
            }
        });
        let summary = outbound_summary("telegram", &telegram, 400).unwrap();
        assert_eq!(
            summary.line,
            "[发送][telegram] 会话(-100123) => [私人消息正文已隐藏]"
        );
        assert!(!summary.line.contains("secret.test"));

        let feishu = json!({
            "action": "reply_message",
            "params": {"message_id": "om_123", "content": "回复内容"}
        });
        let summary = outbound_summary("feishu", &feishu, 400).unwrap();
        assert_eq!(
            summary.line,
            "[发送][feishu] 消息(om_123) => [私人消息正文已隐藏]"
        );

        let qq = json!({
            "action": "send_message",
            "params": {
                "qq_target_type": "group",
                "group_openid": "group-openid",
                "message": "QQ 回复"
            }
        });
        let summary = outbound_summary("qq", &qq, 400).unwrap();
        assert_eq!(
            summary.line,
            "[发送][qq_official] 群(group-openid) => QQ 回复"
        );
    }

    #[test]
    fn text_preview_strips_ansi_and_redacts_sensitive_resources() {
        let base64 = "A".repeat(128);
        let event = json!({
            "post_type": "message",
            "platform": "OneBot v11",
            "message_type": "group",
            "group_id": 42,
            "user_id": 7,
            "sender": {"user_id": 7},
            "raw_message": format!(
                "正常 \u{1b}[31m红色\u{1b}[0m \u{1b}]8;;https://hidden.test\u{1b}\\链接名\u{1b}]8;;\u{1b}\\ https://secret.test /Users/private/key config/secrets.json {base64}"
            )
        });

        let summary = inbound_summary(&event, 400).unwrap();
        assert_eq!(summary.platform, "onebot_v11");
        assert!(
            summary
                .line
                .contains("正常 红色 链接名 [链接] [路径] [路径] [数据]")
        );
        for secret in ["\u{1b}", "hidden.test", "secret.test", "/Users", &base64] {
            assert!(!summary.line.contains(secret));
        }
    }

    #[test]
    fn preview_is_unicode_safe_and_reports_omitted_chars() {
        let event = json!({
            "post_type": "message",
            "message_type": "group",
            "group_id": 42,
            "user_id": 7,
            "sender": {"user_id": 7},
            "raw_message": "天气很好"
        });

        let summary = inbound_summary(&event, 2).unwrap();
        assert!(summary.line.ends_with("天气... (+2 字符)"));
        assert!(summary.truncated);
    }

    #[test]
    fn preview_redacts_relative_paths_and_unpadded_encoded_payloads() {
        let event = json!({
            "post_type": "message",
            "message_type": "group",
            "group_id": 42,
            "user_id": 7,
            "sender": {"user_id": 7},
            "raw_message": "./secrets config/private abcdefghijklmnopqrstuvwxyz0123456789_-AB"
        });

        let summary = inbound_summary(&event, 400).unwrap();
        assert!(summary.line.ends_with("[路径] [路径] [数据]"));
        for secret in ["./secrets", "config/private", "abcdefghijklmnopqrstuvwxyz"] {
            assert!(!summary.line.contains(secret));
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/core/message_log_privacy.rs"]
mod privacy_tests;
