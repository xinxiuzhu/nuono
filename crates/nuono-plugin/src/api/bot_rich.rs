//! 被动回复、路由主动发送与原生 SDK 消息共用的富消息预处理逻辑。
use super::*;
use crate::api::{
    button::{ButtonAction, ButtonOne, ButtonPermission},
    interaction::{InteractionAction, InteractionRegistration},
    routed::{RuntimeMessageTarget, RuntimeTargetKind},
};
use nuono_app::rich_message::{MessagePlan, RichMessage};
#[cfg(test)]
#[path = "../../tests/unit/api/bot_rich_tests.rs"]
mod tests;

fn exact(value: Option<&Value>) -> Option<String> {
    value.and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

impl Bot {
    /// 宿主回复路径在注册完回复等待后，也会复用这套预处理逻辑。
    #[doc(hidden)]
    pub async fn prepare_message(
        &self,
        message: impl Into<SendableMessage>,
    ) -> Result<(Value, Vec<String>)> {
        let (message, degradations) = self.prepare_rich(None, message.into()).await?;
        Ok((message.to_json(), degradations))
    }
    pub(super) fn rich_reply_target(&self) -> Option<RuntimeMessageTarget> {
        let ctx = self.current_message.as_ref()?;
        let raw = &ctx.reply_context;
        let route = raw
            .get("route")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_else(|| {
                RuntimeBotRoute::new("onebot", self.self_id.to_string(), self.self_id.to_string())
            });
        let kind = match ctx.message_type.as_str() {
            "group" => RuntimeTargetKind::Group {
                group_id: exact(raw.get("group_openid"))
                    .or_else(|| exact(raw.get("group_id")))
                    .or_else(|| exact(raw.get("chat_id")))
                    .or_else(|| ctx.group_id.map(|v| v.to_string()))?,
            },
            "channel" => RuntimeTargetKind::Channel {
                channel_id: exact(raw.get("channel_id"))
                    .or_else(|| exact(raw.get("chat_id")))
                    .or_else(|| ctx.channel_id.clone())?,
                parent: exact(raw.get("guild_id")),
            },
            _ => RuntimeTargetKind::Private {
                user_id: exact(raw.get("openid"))
                    .or_else(|| exact(raw.get("user_id")))
                    .unwrap_or_else(|| ctx.user_id.to_string()),
            },
        };
        Some(RuntimeMessageTarget { route, kind })
    }

    pub(super) async fn prepare_rich(
        &self,
        target: Option<&RuntimeMessageTarget>,
        message: SendableMessage,
    ) -> Result<(SendableMessage, Vec<String>)> {
        self.prepare_rich_with_origin(target, message, None).await
    }

    async fn prepare_rich_with_origin(
        &self,
        target: Option<&RuntimeMessageTarget>,
        message: SendableMessage,
        origin_chat_override: Option<String>,
    ) -> Result<(SendableMessage, Vec<String>)> {
        // 媒体消息（图片/视频/语音/文件）没有富消息段，不必为判定而把整条消息编码成
        // JSON：那段编码会把 Base64 深拷贝一份。文本段也不需要宿主处理。
        if !message.may_contain_rich_segments() {
            return Ok((message, Vec::new()));
        }
        let value = message.to_json();
        let Some(mut rich) = RichMessage::from_segments(&value)? else {
            return Ok((message, Vec::new()));
        };
        let current = self.rich_reply_target();
        let target = target.or(current.as_ref());
        let platform = target
            .map(|t| t.route.platform.as_str())
            .unwrap_or("onebot");
        let port = self
            .action_router
            .as_ref()
            .and_then(|r| r.interaction_port());
        let policy = match (&port, target) {
            (Some(port), Some(target)) => port.message_policy(&target.route).await?,
            _ => Default::default(),
        };
        // 即使最终降级为纯文本，指令按钮也必须先按当前插件的指令前缀补全完整指令名。
        for button in rich.buttons.iter_mut().flatten() {
            button.token = None;
            if button.action == ButtonAction::Auto {
                button.action =
                    if button.data.starts_with("https://") || button.data.starts_with("http://") {
                        ButtonAction::Jump
                    } else {
                        ButtonAction::Command
                    };
            }
            if button.action == ButtonAction::Command
                && let (Some(port), Some(plugin)) = (&port, &self.wait_owner)
            {
                button.data = port.resolve_command(plugin, &button.data).await?;
            }
        }
        let mut normalized = nuono_app::rich_message::segments(&value)?;
        normalized.retain(|v| v["type"] != "buttons");
        if !rich.buttons.is_empty() {
            normalized.push(json!({"type":"buttons","data":{"buttons":rich.buttons}}));
        }
        let MessagePlan {
            mut segments,
            mut degradations,
        } = nuono_app::rich_message::plan(platform, &policy, &json!(normalized))?;
        for item in &mut segments {
            if item["type"] != "buttons" {
                continue;
            }
            let mut rows: Vec<Vec<ButtonOne>> =
                serde_json::from_value(item["data"]["buttons"].clone())?;
            let needs_port = rows
                .iter()
                .flatten()
                .any(|b| b.action != ButtonAction::Jump);
            if needs_port && (port.is_none() || target.is_none() || self.wait_owner.is_none()) {
                if policy.fallback == nuono_app::message_interactions::FallbackMode::Error {
                    return Err(NuonoError::Validation(
                        "交互按钮需要宿主插件归属与消息目标".into(),
                    ));
                }
                item["type"] = json!("text");
                item["data"] = json!({"text":nuono_app::rich_message::fallback_options(&rows)});
                degradations.push("缺少交互宿主上下文，已使用文字选项".into());
                continue;
            }
            for button in rows.iter_mut().flatten() {
                let action = match button.action {
                    ButtonAction::Jump => continue,
                    ButtonAction::Reply => InteractionAction::Reply {
                        value: button.data.clone(),
                    },
                    ButtonAction::Callback => InteractionAction::Callback {
                        callback_id: if button.id.is_empty() {
                            button.data.clone()
                        } else {
                            button.id.clone()
                        },
                        data: serde_json::from_str(&button.data)
                            .unwrap_or_else(|_| json!(button.data)),
                    },
                    _ => InteractionAction::Command {
                        command: button.data.clone(),
                    },
                };
                // 当前消息的触发用户与会话元数据仅在发送目标就是当前会话时才适用。
                let context = (target == current.as_ref())
                    .then_some(self.current_message.as_ref())
                    .flatten();
                let actor_id = context
                    .and_then(|ctx| exact(ctx.reply_context.get("user_id")))
                    .or_else(|| match target.map(|target| &target.kind) {
                        Some(RuntimeTargetKind::Private { user_id }) => Some(user_id.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        context
                            .map(|ctx| ctx.user_id.to_string())
                            .unwrap_or_default()
                    });
                let origin_chat_id = origin_chat_override
                    .clone()
                    .or_else(|| context.and_then(|ctx| exact(ctx.reply_context.get("chat_id"))));
                if button.permission == ButtonPermission::SpecifiedUser
                    && button.specify_user_ids.is_empty()
                {
                    if actor_id.trim().is_empty() {
                        return Err(NuonoError::Validation(
                            "指定用户按钮需要明确的用户列表或当前消息用户".into(),
                        ));
                    }
                    // 确保宿主鉴权列表与传给平台原生按钮权限字段的用户列表保持一致。
                    button.specify_user_ids.push(actor_id.clone());
                }
                button.token = Some(
                    port.as_ref()
                        .expect("checked")
                        .register(InteractionRegistration {
                            plugin: self.wait_owner.clone().expect("checked"),
                            target: target.expect("checked").clone(),
                            actor_id,
                            origin_chat_id,
                            permission: button.permission,
                            allowed_user_ids: button.specify_user_ids.clone(),
                            action,
                            ttl_secs: button.ttl_secs,
                        })
                        .await?,
                );
            }
            item["data"]["buttons"] = json!(rows);
        }
        if !degradations.is_empty() {
            tracing::info!(plugin=self.wait_owner.as_deref().unwrap_or(""),platform,degradations=?degradations,"富消息按能力降级发送");
        }
        Ok((
            SendableMessage::Json(serde_json::to_string(&segments)?),
            degradations,
        ))
    }

    /// 发送显式声明的平台原生 Markdown 消息（普通文本消息不会受此影响）。
    pub async fn send_markdown(&self, content: impl Into<String>) -> Result<()> {
        self.send(crate::api::MessageSegment::markdown(content))
            .await
    }

    /// 发送平台已审核通过的 Markdown 模板消息；必须同时提供可读的纯文本回退内容（`fallback`），供不支持模板的平台降级使用。
    pub async fn send_template_markdown(
        &self,
        template_id: impl Into<String>,
        params: serde_json::Map<String, Value>,
        fallback: impl Into<String>,
    ) -> Result<()> {
        self.send(SendableMessage::Json(json!([{"type":"template_markdown","data":{"template_id":template_id.into(),"params":params,"fallback":fallback.into()}}]).to_string())).await
    }

    /// 按消息策略配置的列数自动将一维按钮列表排版成多行（若直接使用二维按钮行则保持原样）。
    pub async fn send_options(
        &self,
        message: impl Into<SendableMessage>,
        buttons: Vec<ButtonOne>,
    ) -> Result<()> {
        let target = self.rich_reply_target();
        let port = self
            .action_router
            .as_ref()
            .and_then(|r| r.interaction_port());
        let columns = if let (Some(port), Some(target)) = (port, target) {
            port.message_policy(&target.route).await?.button_columns as usize
        } else {
            2
        };
        let mut segments = nuono_app::rich_message::segments(&message.into().to_json())?;
        let rows = buttons
            .chunks(columns)
            .map(|row| row.to_vec())
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            segments.push(json!({"type":"buttons","data":{"buttons":rows}}));
        }
        self.send(SendableMessage::Json(json!(segments).to_string()))
            .await
    }

    /// 发送由管理员配置且经平台审核通过的静态按钮模板。
    /// 注意：静态按钮模板不包含宿主动态生成的交互回调令牌。
    pub async fn send_button_template(&self, message: impl Into<SendableMessage>) -> Result<()> {
        let target = self
            .rich_reply_target()
            .ok_or(NuonoError::MessageContextNotSet)?;
        let port = self
            .action_router
            .as_ref()
            .and_then(|router| router.interaction_port())
            .ok_or_else(|| NuonoError::Validation("缺少消息设置宿主".into()))?;
        let policy = port.message_policy(&target.route).await?;
        if policy.button_template_id.is_empty() {
            return Err(NuonoError::Validation("尚未配置按钮模板 ID".into()));
        }
        let mut items = nuono_app::rich_message::segments(&message.into().to_json())?;
        items.push(json!({"type":"keyboard","data":{"id":policy.button_template_id}}));
        self.send(SendableMessage::Json(json!(items).to_string()))
            .await
    }
}

impl Bot {
    pub(super) async fn prepare_native_rich(
        &self,
        route: &RuntimeBotRoute,
        action: crate::domain::OutboundAction,
    ) -> Result<(crate::domain::OutboundAction, Vec<String>)> {
        use crate::domain::{ConversationRef, DeliveryTarget, MessageSegment, OutboundAction};
        let OutboundAction::SendMessage {
            target,
            segments,
            reply_to,
        } = action
        else {
            return Ok((action, Vec::new()));
        };
        let direct_origin = match &target {
            DeliveryTarget::Conversation {
                conversation:
                    ConversationRef::Direct {
                        conversation: Some(conversation),
                        ..
                    },
            } => Some(conversation.value().to_string()),
            _ => None,
        };
        let kind = match &target {
            DeliveryTarget::Actor { actor } => RuntimeTargetKind::Private {
                user_id: actor.value().to_string(),
            },
            DeliveryTarget::Conversation {
                conversation: ConversationRef::Direct { participant, .. },
            } => RuntimeTargetKind::Private {
                user_id: participant.value().to_string(),
            },
            DeliveryTarget::Conversation {
                conversation: ConversationRef::Group { group },
            } => RuntimeTargetKind::Group {
                group_id: group.value().to_string(),
            },
            DeliveryTarget::Conversation {
                conversation: ConversationRef::Channel { channel, parent },
            }
            | DeliveryTarget::Channel { channel, parent } => RuntimeTargetKind::Channel {
                channel_id: channel.value().to_string(),
                parent: parent.as_ref().map(|p| p.value().to_string()),
            },
            _ => {
                return Err(NuonoError::Validation("不支持的富文本投递目标类型".into()));
            }
        };
        let value = json!(
            segments
                .iter()
                .map(crate::domain::projection::segment_json)
                .collect::<Vec<_>>()
        );
        if RichMessage::from_segments(&value)?.is_none() {
            return Ok((
                OutboundAction::SendMessage {
                    target,
                    segments,
                    reply_to,
                },
                Vec::new(),
            ));
        }
        let legacy = RuntimeMessageTarget {
            route: route.clone(),
            kind,
        };
        let (prepared, degradations) = self
            .prepare_rich_with_origin(
                Some(&legacy),
                SendableMessage::Json(value.to_string()),
                direct_origin,
            )
            .await?;
        let items = nuono_app::rich_message::segments(&prepared.to_json())?;
        let mut output = Vec::new();
        for item in items {
            let kind = item["type"].as_str().unwrap_or_default();
            let data = item.get("data").cloned().unwrap_or(Value::Null);
            output.push(match kind {
                "text" => MessageSegment::text(data["text"].as_str().unwrap_or_default()),
                "markdown" => MessageSegment::Markdown {
                    content: data["content"].as_str().unwrap_or_default().into(),
                },
                "buttons" => MessageSegment::Buttons {
                    buttons: serde_json::from_value(data["buttons"].clone())?,
                },
                "template_markdown" => MessageSegment::TemplateMarkdown {
                    template_id: data["template_id"].as_str().unwrap_or_default().into(),
                    data: json!({"params":data["params"],"fallback":data["fallback"]}),
                },
                _ => segments
                    .iter()
                    .find(|s| crate::domain::projection::segment_json(s) == item)
                    .cloned()
                    .unwrap_or(MessageSegment::Unknown {
                        kind: kind.into(),
                        data,
                    }),
            });
        }
        Ok((
            OutboundAction::SendMessage {
                target,
                segments: output,
                reply_to,
            },
            degradations,
        ))
    }
}
