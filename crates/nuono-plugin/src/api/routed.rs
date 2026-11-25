//! 跨平台 routed 主动消息发送 SDK。
//!
//! 插件在后台任务里保存一个 [`RuntimeMessageTarget`]，随后用
//! [`Bot::send_routed`](crate::api::Bot::send_routed) 主动发送消息——不依赖当前消息
//! 上下文、不使用数字 ID、不让插件拼 adapter-specific action。框架按目标 route 的
//! platform 生成各 sink 能解析的规范 `send_msg` action，并走
//! [`Bot::call_routed_receipt`](crate::api::Bot::call_routed_receipt) 拿回精确的原生
//! 消息 ID 与原始回包。

use crate::api::bot::SendableMessage;
use crate::core::RuntimeBotRoute;
use crate::error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 目标消息的语义类型：私聊 / 群聊 / 频道。
///
/// 所有原生 ID 都以字符串保真；Telegram 的负数 chat id、Discord 的超 i64
/// snowflake、Feishu/QQ 的 openid 都原样保存，不做数字转换。频道类目标携带可选的
/// parent (guild) 字符串，为 thread 之类的扩展留空间，但不臆造平台字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RuntimeTargetKind {
    /// 私聊 / 单聊。
    Private {
        /// 平台原生用户 ID（Telegram user id、Discord snowflake、Feishu/QQ openid）。
        user_id: String,
    },
    /// 群聊。
    Group {
        /// 平台原生群 ID（Telegram group id、Discord 文本频道、Feishu/QQ openid）。
        group_id: String,
    },
    /// 频道子区（Discord 频道、Telegram 频道等）。
    Channel {
        /// 平台原生频道 ID。
        channel_id: String,
        /// 可选的父级 / guild ID（Discord guild、QQ 群等），给 thread 扩展留空间。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
}

/// 跨平台字符串身份保真的主动消息目标。
///
/// 内嵌精确实时的 [`RuntimeBotRoute`]，因此 platform 的单一真相来自 route——目标
/// 自身不再重复携带 platform，避免双真相。serde roundtrip 稳定，可直接存插件
/// subscriptions 表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeMessageTarget {
    /// 精确到实例与 bot 身份的实时路由。
    pub route: RuntimeBotRoute,
    /// 目标语义类型与原生字符串 ID。
    pub kind: RuntimeTargetKind,
}

impl RuntimeMessageTarget {
    /// 构造私聊目标。
    ///
    /// Discord 传入原生用户 ID，适配器先取得 DM 频道再发送；已有频道 ID 使用
    /// [`Self::channel`]。持久化目标保存用户 ID，连接实例重新建立后仍可解析。
    pub fn private(route: RuntimeBotRoute, user_id: impl Into<String>) -> Self {
        Self {
            route,
            kind: RuntimeTargetKind::Private {
                user_id: user_id.into(),
            },
        }
    }

    /// 构造群聊目标。
    pub fn group(route: RuntimeBotRoute, group_id: impl Into<String>) -> Self {
        Self {
            route,
            kind: RuntimeTargetKind::Group {
                group_id: group_id.into(),
            },
        }
    }

    /// 构造频道目标。`parent` 为可选的父级 / guild ID。
    pub fn channel(
        route: RuntimeBotRoute,
        channel_id: impl Into<String>,
        parent: Option<impl Into<String>>,
    ) -> Self {
        Self {
            route,
            kind: RuntimeTargetKind::Channel {
                channel_id: channel_id.into(),
                parent: parent.map(Into::into),
            },
        }
    }

    /// 路由的只读访问器。
    pub fn route(&self) -> &RuntimeBotRoute {
        &self.route
    }

    /// 目标语义类型的只读访问器。
    pub fn kind(&self) -> &RuntimeTargetKind {
        &self.kind
    }

    /// 校验目标：route 三字段非空、目标 ID 非空、可选 parent 非空。
    ///
    /// platform 无需在此与目标比对：目标的平台单一真相即 `route.platform`，目标自身
    /// 不携带 platform，因此不存在显式不一致的可能。
    pub fn validate(&self) -> Result<()> {
        if self.route.platform.trim().is_empty() {
            return Err(NuonoError::Validation(
                "runtime route platform 不能为空".to_string(),
            ));
        }
        if self.route.instance_id.trim().is_empty() {
            return Err(NuonoError::Validation(
                "runtime route instance_id 不能为空".to_string(),
            ));
        }
        if self.route.bot_self_id.trim().is_empty() {
            return Err(NuonoError::Validation(
                "runtime route bot_self_id 不能为空".to_string(),
            ));
        }
        match &self.kind {
            RuntimeTargetKind::Private { user_id } => {
                reject_empty(user_id.as_str(), "目标 user_id")
            }
            RuntimeTargetKind::Group { group_id } => {
                reject_empty(group_id.as_str(), "目标 group_id")
            }
            RuntimeTargetKind::Channel { channel_id, parent } => {
                reject_empty(channel_id.as_str(), "目标 channel_id")?;
                if let Some(parent) = parent {
                    reject_empty(parent.as_str(), "频道 parent")?;
                }
                Ok(())
            }
        }
    }
}

fn reject_empty(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(NuonoError::Validation(format!("{field} 不能为空")));
    }
    Ok(())
}

/// 构建 framework 规范的 `send_msg` action。
///
/// 目标参数按 route platform 归一化为该平台 sink 实际能解析的公共形状
/// （`message_type` + 对应目标键 + `message`）。Telegram 的 ID 本质是数字，此处显式
/// 校验并转换为 i64；其余平台一律保留字符串，不静默丢失精度。
pub fn build_send_msg_action(
    platform: &str,
    target: &RuntimeMessageTarget,
    message: &SendableMessage,
) -> Result<Value> {
    send_msg_params(platform, target, message.to_json()).map(send_msg_action)
}

/// [`build_send_msg_action`] 的所有权版本：消息段按移动编码，绝不深拷贝媒体字段。
pub fn build_send_msg_action_owned(
    platform: &str,
    target: &RuntimeMessageTarget,
    message: SendableMessage,
) -> Result<Value> {
    send_msg_params(platform, target, message.into_json()).map(send_msg_action)
}

fn send_msg_action(params: Value) -> Value {
    let mut action = serde_json::Map::with_capacity(2);
    action.insert("action".into(), Value::String("send_msg".into()));
    action.insert("params".into(), params);
    Value::Object(action)
}

/// 用移动语义拼装参数对象；`message` 是消息段数组，可能携带数十 MB 的 Base64。
fn message_params(entries: &[(&str, Value)], message: Value) -> Value {
    let mut params = serde_json::Map::with_capacity(entries.len() + 1);
    for (key, value) in entries {
        params.insert((*key).into(), value.clone());
    }
    params.insert("message".into(), message);
    Value::Object(params)
}

fn send_msg_params(platform: &str, target: &RuntimeMessageTarget, msg: Value) -> Result<Value> {
    let platform = platform.to_ascii_lowercase();
    let telegram = platform == "telegram";
    let known_qq = matches!(platform.as_str(), "qq_official" | "qq");
    let discord = platform == "discord";
    let feishu = platform == "feishu";

    match &target.kind {
        RuntimeTargetKind::Private { user_id } => {
            if telegram {
                Ok(message_params(
                    &[
                        ("message_type", json!("private")),
                        ("user_id", json!(telegram_i64(user_id)?)),
                    ],
                    msg,
                ))
            } else if known_qq {
                Ok(message_params(
                    &[
                        ("message_type", json!("private")),
                        ("openid", json!(user_id)),
                        ("user_id", json!(user_id)),
                        ("qq_target_type", json!("c2c")),
                    ],
                    msg,
                ))
            } else if discord {
                Ok(message_params(
                    &[
                        ("message_type", json!("private")),
                        ("user_id", json!(user_id)),
                    ],
                    msg,
                ))
            } else if feishu {
                Ok(message_params(
                    &[
                        ("message_type", json!("private")),
                        ("chat_id", json!(user_id)),
                    ],
                    msg,
                ))
            } else {
                Ok(message_params(
                    &[
                        ("message_type", json!("private")),
                        ("user_id", json!(user_id)),
                    ],
                    msg,
                ))
            }
        }
        RuntimeTargetKind::Group { group_id } => {
            if telegram {
                Ok(message_params(
                    &[
                        ("message_type", json!("group")),
                        ("group_id", json!(telegram_i64(group_id)?)),
                    ],
                    msg,
                ))
            } else if known_qq {
                Ok(message_params(
                    &[
                        ("message_type", json!("group")),
                        ("group_openid", json!(group_id)),
                        ("group_id", json!(group_id)),
                        ("qq_target_type", json!("group")),
                    ],
                    msg,
                ))
            } else if discord {
                Ok(message_params(
                    &[
                        ("message_type", json!("group")),
                        ("channel_id", json!(group_id)),
                    ],
                    msg,
                ))
            } else if feishu {
                Ok(message_params(
                    &[
                        ("message_type", json!("group")),
                        ("chat_id", json!(group_id)),
                    ],
                    msg,
                ))
            } else {
                Ok(message_params(
                    &[
                        ("message_type", json!("group")),
                        ("group_id", json!(group_id)),
                    ],
                    msg,
                ))
            }
        }
        RuntimeTargetKind::Channel { channel_id, parent } => {
            if telegram {
                Ok(message_params(
                    &[
                        ("message_type", json!("channel")),
                        ("chat_id", json!(telegram_i64(channel_id)?)),
                    ],
                    msg,
                ))
            } else {
                let mut entries = vec![
                    ("message_type", json!("channel")),
                    ("channel_id", json!(channel_id)),
                ];
                if let Some(parent) = parent {
                    entries.push(("guild_id", json!(parent)));
                }
                Ok(message_params(&entries, msg))
            }
        }
    }
}

fn telegram_i64(value: &str) -> Result<i64> {
    value
        .trim()
        .parse::<i64>()
        .map_err(|_| NuonoError::Validation(format!("Telegram 目标 ID 必须是整数，收到: {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::bot::SendableMessage;
    use crate::api::segment::MessageSegment;
    use crate::core::RuntimeBotRoute;

    fn route(platform: &str) -> RuntimeBotRoute {
        RuntimeBotRoute::new(platform, "instance-a", "bot-self")
    }

    /// 所有权版编码器的设计目的是避免对大段媒体数据做深拷贝；
    /// 但它在每个平台分支上生成的线上 JSON 必须与借用版编码器完全一致。
    #[test]
    fn owned_and_borrowed_send_actions_are_byte_identical() {
        let targets = [
            RuntimeMessageTarget::private(route("telegram"), "42"),
            RuntimeMessageTarget::group(route("qq_official"), "group-openid"),
            RuntimeMessageTarget::group(route("discord"), "channel-snowflake"),
            RuntimeMessageTarget::private(route("feishu"), "ou_0123"),
            RuntimeMessageTarget::channel(route("onebot"), "chan", Some("guild")),
            RuntimeMessageTarget::group(route("onebot"), "100001"),
        ];
        for target in targets {
            let message = SendableMessage::Segments(vec![
                MessageSegment::text("标题"),
                MessageSegment::video(format!("base64://{}", "A".repeat(64))),
                MessageSegment::at_all(),
            ]);
            let borrowed =
                build_send_msg_action(&target.route.platform, &target, &message).unwrap();
            let owned =
                build_send_msg_action_owned(&target.route.platform, &target, message).unwrap();
            assert_eq!(borrowed, owned, "{} diverged", target.route.platform);
        }
    }

    #[test]
    fn owned_encoding_moves_large_media_instead_of_copying_it() {
        let target = RuntimeMessageTarget::group(route("onebot"), "100001");
        let payload = format!("base64://{}", "A".repeat(1024));
        let message = SendableMessage::Segment(MessageSegment::video(payload.clone()));
        let action = build_send_msg_action_owned(&target.route.platform, &target, message).unwrap();
        assert_eq!(
            action["params"]["message"][0]["data"]["file"]
                .as_str()
                .unwrap(),
            payload
        );
        assert_eq!(action["params"]["message"][0]["type"], "video");
    }

    // serde roundtrip：Telegram 负数、Discord 超 i64 snowflake、Feishu/QQ openid。
    #[test]
    fn serde_roundtrip_preserves_exact_string_ids() {
        let targets = vec![
            RuntimeMessageTarget::private(route("telegram"), "-1001234567890"),
            RuntimeMessageTarget::private(route("discord"), "18446744073709551615"),
            RuntimeMessageTarget::private(route("feishu"), "ou_9bcdef0123456789"),
            RuntimeMessageTarget::private(route("qq_official"), "member-openid"),
            RuntimeMessageTarget::group(route("qq_official"), "group-openid"),
            RuntimeMessageTarget::channel(route("discord"), "channel-snowflake", None::<String>),
            RuntimeMessageTarget::channel(
                route("discord"),
                "thread-snowflake",
                Some("guild-snowflake"),
            ),
        ];
        for target in targets {
            let encoded = serde_json::to_string(&target).expect("encode");
            let restored: RuntimeMessageTarget = serde_json::from_str(&encoded).expect("decode");
            assert_eq!(restored, target);
        }
    }

    #[test]
    fn channel_parent_is_optional_and_roundtrips() {
        let no_parent = RuntimeMessageTarget::channel(route("discord"), "c1", None::<String>);
        let encoded = serde_json::to_string(&no_parent).unwrap();
        assert!(!encoded.contains("parent"));
        let restored: RuntimeMessageTarget = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, no_parent);
    }

    #[test]
    fn validate_rejects_empty_route_and_empty_ids() {
        let empty_platform =
            RuntimeMessageTarget::private(RuntimeBotRoute::new("", "i", "b"), "u1");
        assert!(empty_platform.validate().is_err());

        let empty_id = RuntimeMessageTarget::private(route("telegram"), " ");
        assert!(empty_id.validate().is_err());

        let empty_channel = RuntimeMessageTarget::channel(route("discord"), " ", None::<String>);
        assert!(empty_channel.validate().is_err());
    }

    // 规范 action 形状（private / group / channel 各平台）。
    #[test]
    fn private_action_shape_per_platform() {
        let target = RuntimeMessageTarget::private(route("telegram"), "-1001234567890");
        let action =
            build_send_msg_action("telegram", &target, &SendableMessage::from("hi")).unwrap();
        assert_eq!(action["action"], "send_msg");
        assert_eq!(action["params"]["user_id"], -1001234567890_i64);
        assert_eq!(
            action["params"]["message"],
            json!([{"type": "text", "data": {"text": "hi"}}])
        );

        let qq = RuntimeMessageTarget::private(route("qq_official"), "member-openid");
        let action =
            build_send_msg_action("qq_official", &qq, &SendableMessage::from("hi")).unwrap();
        assert_eq!(action["params"]["openid"], "member-openid");
        assert_eq!(action["params"]["qq_target_type"], "c2c");

        let discord = RuntimeMessageTarget::private(route("discord"), "18446744073709551615");
        let action =
            build_send_msg_action("discord", &discord, &SendableMessage::from("hi")).unwrap();
        assert_eq!(action["params"]["user_id"], "18446744073709551615");
        assert!(action["params"].get("channel_id").is_none());
    }

    #[test]
    fn group_action_shape_per_platform() {
        let qq = RuntimeMessageTarget::group(route("qq_official"), "group-openid");
        let action =
            build_send_msg_action("qq_official", &qq, &SendableMessage::from("hi")).unwrap();
        assert_eq!(action["params"]["group_openid"], "group-openid");
        assert_eq!(action["params"]["qq_target_type"], "group");

        let tg = RuntimeMessageTarget::group(route("telegram"), "-1001234567890");
        let action = build_send_msg_action("telegram", &tg, &SendableMessage::from("hi")).unwrap();
        assert_eq!(action["params"]["group_id"], -1001234567890_i64);
    }

    #[test]
    fn channel_action_shape_per_platform() {
        let discord = RuntimeMessageTarget::channel(
            route("discord"),
            "channel-snowflake",
            Some("guild-snowflake"),
        );
        let action =
            build_send_msg_action("discord", &discord, &SendableMessage::from("hi")).unwrap();
        assert_eq!(action["params"]["channel_id"], "channel-snowflake");
        assert_eq!(action["params"]["guild_id"], "guild-snowflake");
    }

    #[test]
    fn rich_segments_and_image_are_not_lost() {
        let target = RuntimeMessageTarget::group(route("qq_official"), "group-openid");
        let message: SendableMessage = vec![
            MessageSegment::text("hello "),
            MessageSegment::image("file:///tmp/a.jpg"),
        ]
        .into();
        let action = build_send_msg_action("qq_official", &target, &message).unwrap();
        let segments = action["params"]["message"].as_array().expect("array");
        assert_eq!(segments[0]["type"], "text");
        assert_eq!(segments[0]["data"]["text"], "hello ");
        assert_eq!(segments[1]["type"], "image");
        assert_eq!(segments[1]["data"]["file"], "file:///tmp/a.jpg");
    }

    #[test]
    fn telegram_rejects_non_numeric_id() {
        let target = RuntimeMessageTarget::private(route("telegram"), "abc");
        let err =
            build_send_msg_action("telegram", &target, &SendableMessage::from("hi")).unwrap_err();
        assert!(matches!(err, NuonoError::Validation(_)));
    }

    // OneBot 走 core builder 的非特化分支（platform 非 telegram/qq/discord/feishu），
    // 产出规范 send_msg 的三类目标参数，sink 原样透传。此处锁定该形状。
    #[test]
    fn onebot_fallback_exported_plain_id_params() {
        let private = RuntimeMessageTarget::private(route("onebot"), "10086");
        let action =
            build_send_msg_action("onebot", &private, &SendableMessage::from("hi")).unwrap();
        assert_eq!(action["params"]["message_type"], "private");
        assert_eq!(action["params"]["user_id"], "10086");
        assert_eq!(
            action["params"]["message"],
            json!([{"type": "text", "data": {"text": "hi"}}])
        );

        let group = RuntimeMessageTarget::group(route("onebot"), "20001");
        let action = build_send_msg_action("onebot", &group, &SendableMessage::from("hi")).unwrap();
        assert_eq!(action["params"]["message_type"], "group");
        assert_eq!(action["params"]["group_id"], "20001");

        let channel = RuntimeMessageTarget::channel(route("onebot"), "30001", None::<String>);
        let action =
            build_send_msg_action("onebot", &channel, &SendableMessage::from("hi")).unwrap();
        assert_eq!(action["params"]["message_type"], "channel");
        assert_eq!(action["params"]["channel_id"], "30001");
        assert!(action["params"].get("guild_id").is_none());
    }
}
