//! 将现有的平台 `RuntimeActionSink` 包装为强类型 `ActionTransport` 适配器，在编解码全程保留精确的原生字符串 ID。
use super::{RuntimeActionSink, RuntimeBotRoute};
use nuono_app::transport::{ActionTransport, ConnectionId, ProtocolDescriptor, TransportAction};
use nuono_domain::*;
use serde_json::{Value, json};
use std::sync::Arc;
pub struct SinkTransport {
    route: RuntimeBotRoute,
    message_policy: Option<crate::api::interaction::InteractionPortSource>,
    id: ConnectionId,
    protocol: ProtocolDescriptor,
    bot: BotIdentity,
    sink: Arc<dyn RuntimeActionSink>,
    audit: Option<Arc<dyn nuono_app::action::ActionAudit>>,
}

impl std::fmt::Debug for SinkTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SinkTransport")
            .field("route", &self.route)
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("bot", &self.bot)
            .finish_non_exhaustive()
    }
}
impl SinkTransport {
    pub fn new(
        route: &RuntimeBotRoute,
        generation: u64,
        sink: Arc<dyn RuntimeActionSink>,
    ) -> Result<Self, DomainModelError> {
        let bot = route_identity(route)?;
        Ok(Self {
            route: route.clone(),
            message_policy: None,
            id: ConnectionId::new(format!(
                "{}:{generation}",
                serde_json::to_string(route).expect("route serializes")
            ))?,
            protocol: ProtocolDescriptor::new(
                &route.platform,
                if route.platform == "onebot" {
                    "11"
                } else {
                    "1"
                },
            )?,
            bot,
            sink,
            audit: None,
        })
    }
    pub fn with_audit(mut self, audit: Arc<dyn nuono_app::action::ActionAudit>) -> Self {
        self.audit = Some(audit);
        self
    }
    pub fn with_message_policy_source(
        mut self,
        source: crate::api::interaction::InteractionPortSource,
    ) -> Self {
        self.message_policy = Some(source);
        self
    }

    async fn native_file_request(
        &self,
        action: &TransportAction,
    ) -> Result<nuono_app::media::NativeFileSendRequest, ActionError> {
        if action.bot != self.bot {
            return Err(rejected("transport bot identity mismatch"));
        }
        let OutboundAction::SendFile {
            target,
            source,
            file_name,
            content_type,
        } = &action.action
        else {
            return Err(rejected("native file action required"));
        };
        let FileSource::Base64(source) = source else {
            return Err(rejected("native file delivery requires bounded file bytes"));
        };
        let base64 = source
            .strip_prefix("base64://")
            .ok_or_else(|| rejected("native file bytes must use the base64 source format"))?;
        let request = nuono_app::media::NativeFileSendRequest {
            target: target.clone(),
            base64: base64.to_owned(),
            file_name: file_name.clone(),
            content_type: content_type.clone(),
        };
        request
            .validate_for_platform(self.bot.platform())
            .map_err(|error| ActionError::new(ActionErrorKind::Rejected, error.to_string()))?;
        if matches!(self.bot.platform(), Platform::QqOfficial | Platform::Feishu) {
            request
                .validate_for_application(self.bot.platform(), self.bot.account_id.value().as_str())
                .map_err(|error| ActionError::new(ActionErrorKind::Rejected, error.to_string()))?;
        }
        // 目标校验失败（包括适配器发现目标身份体系/作用域与当前客户端不匹配）必须直接报错拒绝，
        // 绝不能当作“不支持文件发送能力”而降级；只有底层文件接口明确返回 `Unsupported` 时才允许降级。
        self.sink
            .validate_native_target(target)
            .await
            .map_err(|error| ActionError::new(ActionErrorKind::Rejected, error.to_string()))?;
        Ok(request)
    }
    async fn encode(&self, action: &mut TransportAction) -> Result<Value, ActionError> {
        if action.bot != self.bot {
            return Err(ActionError::new(
                ActionErrorKind::Rejected,
                "transport bot identity mismatch",
            ));
        }
        // 先克隆一份轻量的发送目标（DeliveryTarget），避免后续对 `action.action` 做可变借用编码时产生借用冲突；
        // 真正体积庞大、绝不能被深拷贝的是消息正文（segments / params）。
        let destination = match &action.action {
            OutboundAction::SendMessage { target, .. }
            | OutboundAction::EditMessage { target, .. }
            | OutboundAction::React { target, .. }
            | OutboundAction::Moderate { target, .. } => Some(target.clone()),
            OutboundAction::DeleteMessage { target, .. } => target.clone(),
            _ => None,
        };
        if let Some(destination) = &destination {
            validate_target(destination, &self.bot)?;
            self.sink
                .validate_native_target(destination)
                .await
                .map_err(|error| {
                    let kind = if matches!(error, crate::error::NuonoError::NotImplemented(_)) {
                        ActionErrorKind::UnsupportedCapability
                    } else {
                        ActionErrorKind::Rejected
                    };
                    ActionError::new(kind, error.to_string())
                })?;
        }
        if let OutboundAction::Query {
            query:
                QueryOperation::GroupMember {
                    conversation,
                    actor,
                },
        } = &action.action
        {
            for destination in [
                DeliveryTarget::conversation(conversation.as_ref().clone())
                    .map_err(rejected_domain)?,
                DeliveryTarget::actor(actor.clone()).map_err(rejected_domain)?,
            ] {
                validate_target(&destination, &self.bot)?;
                self.sink
                    .validate_native_target(&destination)
                    .await
                    .map_err(|error| {
                        ActionError::new(ActionErrorKind::Rejected, error.to_string())
                    })?;
            }
        }
        let mut value = encode(&mut action.action)?;
        // 原生平台适配器通过 `params.reply_to` / `params.message_id` 传递引用回复 ID；
        // 只有 OneBot 才需要在 `message` 数组开头塞一个 `reply` 消息段。
        // 若在原生平台上保留该 `reply` 段，富消息规划器会把它误判为普通附件，导致原生 Markdown 和交互按钮被不必要地降级。
        if self.bot.platform() != &Platform::OneBot
            && matches!(
                &action.action,
                OutboundAction::SendMessage {
                    reply_to: Some(_),
                    ..
                }
            )
            && value["params"]["message"]
                .as_array()
                .is_some_and(|message| {
                    message.iter().any(|segment| {
                        matches!(
                            segment["type"].as_str(),
                            Some("markdown" | "template_markdown" | "buttons" | "keyboard")
                        )
                    })
                })
            && let Some(message) = value["params"]["message"].as_array_mut()
        {
            message.remove(0);
        }
        if matches!(
            action.action,
            OutboundAction::SendMessage { .. } | OutboundAction::EditMessage { .. }
        ) && let Some(message) = value.get("params").and_then(|params| params.get("message"))
            && nuono_app::rich_message::RichMessage::from_segments(message)
                .map_err(error)?
                .is_some()
        {
            let port = self
                .message_policy
                .as_ref()
                .and_then(|source| source.read().expect("锁已中毒").clone());
            let policy = match port {
                Some(port) => port.message_policy(&self.route).await.map_err(error)?,
                None => Default::default(),
            };
            let mut planned = nuono_app::rich_message::plan(&self.route.platform, &policy, message)
                .map_err(error)?;
            for segment in &mut planned.segments {
                if segment["type"] == "buttons" {
                    let rows: Vec<Vec<nuono_domain::ButtonOne>> =
                        serde_json::from_value(segment["data"]["buttons"].clone()).map_err(
                            |error| ActionError::new(ActionErrorKind::Rejected, error.to_string()),
                        )?;
                    if rows.iter().flatten().any(|button| {
                        button.action != nuono_domain::ButtonAction::Jump && button.token.is_none()
                    }) {
                        if policy.fallback == nuono_app::message_interactions::FallbackMode::Error {
                            return Err(rejected(
                                "interactive buttons require a host-registered SDK invocation",
                            ));
                        }
                        *segment = json!({"type":"text","data":{"text":nuono_app::rich_message::fallback_options(&rows)}});
                        planned
                            .degradations
                            .push("交互按钮缺少宿主登记，已使用文字选项".into());
                    }
                }
            }
            value["params"]["message"] = json!(planned.segments);
            if !planned.degradations.is_empty() {
                value["_nuono_rich_degradations"] = json!(planned.degradations);
            }
        }
        if self.bot.platform() == &Platform::Feishu
            && let Some(destination) = &destination
        {
            let recipient = feishu_recipient(destination);
            value["params"]["native_receive_id"] = json!(recipient.value().as_str());
            value["params"]["native_receive_id_type"] = json!(match recipient.scheme() {
                IdentityScheme::OpenId => "open_id",
                IdentityScheme::UserId => "user_id",
                IdentityScheme::UnionId => "union_id",
                IdentityScheme::ChatId => "chat_id",
                _ => unreachable!("validated Feishu scheme"),
            });
        }
        if matches!(self.bot.platform(), Platform::QqOfficial)
            && matches!(
                action.action,
                OutboundAction::SendMessage { .. } | OutboundAction::DeleteMessage { .. }
            )
        {
            let params = &mut value["params"];
            // QQ 官方平台只接受原生字符串形式的 ID；前方的 `id()` 辅助函数仅在数字格式化回字符串完全无损时才会转为整数，
            // 因此这里把整数重新转回字符串不会丢失前导零或改变原始拼写。
            for key in ["user_id", "group_id", "reply_to", "message_id"] {
                if let Some(number) = params.get(key).and_then(Value::as_i64) {
                    params[key] = json!(number.to_string());
                }
            }
            match params.get("message_type").and_then(Value::as_str) {
                Some("private") => {
                    params["openid"] = params["user_id"].clone();
                    params["qq_target_type"] = json!("c2c");
                }
                Some("group") => {
                    params["group_openid"] = params["group_id"].clone();
                    params["qq_target_type"] = json!("group");
                }
                _ => {}
            }
        }
        Ok(value)
    }
}

fn rejected(message: &'static str) -> ActionError {
    ActionError::new(ActionErrorKind::Rejected, message)
}

fn take_rich_degradations(encoded: &mut Value) -> Vec<String> {
    encoded
        .as_object_mut()
        .and_then(|object| object.remove("_nuono_rich_degradations"))
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn rejected_domain(error: DomainModelError) -> ActionError {
    ActionError::new(ActionErrorKind::Rejected, error.to_string())
}

/// 在把目标中的各个 `IdentityRef` 编码为旧版兼容字段前，先校验其平台、身份体系（scheme）与命名空间是否合法。
fn validate_target(target: &DeliveryTarget, bot: &BotIdentity) -> Result<(), ActionError> {
    target.validate().map_err(rejected_domain)?;
    if target.platform() != bot.platform() {
        return Err(rejected("native target platform does not match bot"));
    }
    for identity in target.identities() {
        if identity.platform() != bot.platform() {
            return Err(rejected("native target contains a different platform"));
        }
        match bot.platform() {
            Platform::QqOfficial => {
                if *identity.scheme() != IdentityScheme::OpenId
                    || identity.namespace() != Some(bot.account_id.value())
                    || !matches!(identity.kind(), IdentityKind::User | IdentityKind::Group)
                {
                    return Err(rejected(
                        "QQ native target is not an application-scoped C2C user or group",
                    ));
                }
            }
            Platform::Feishu => {
                if identity.namespace() != Some(bot.account_id.value()) {
                    return Err(rejected(
                        "Feishu native target belongs to another application",
                    ));
                }
                let valid = match identity.kind() {
                    IdentityKind::User => matches!(
                        identity.scheme(),
                        IdentityScheme::OpenId | IdentityScheme::UserId | IdentityScheme::UnionId
                    ),
                    IdentityKind::Conversation | IdentityKind::Group | IdentityKind::Channel => {
                        *identity.scheme() == IdentityScheme::ChatId
                    }
                    _ => false,
                };
                if !valid {
                    return Err(rejected("unsupported Feishu native target scheme"));
                }
            }
            Platform::OneBot | Platform::Other(_) => {
                if *identity.scheme() != IdentityScheme::Native || identity.namespace().is_some() {
                    return Err(rejected("unsupported native target scheme or namespace"));
                }
            }
            Platform::Telegram => {
                if *identity.scheme() != IdentityScheme::Numeric
                    || identity.namespace().is_some()
                    || !identity.value().as_str().parse::<i64>().is_ok_and(|value| {
                        value != 0
                            && (!matches!(
                                identity.kind(),
                                IdentityKind::User | IdentityKind::Conversation
                            ) || value > 0)
                            && value.to_string() == identity.value().as_str()
                    })
                {
                    return Err(rejected(
                        "Telegram target must be an exact numeric identity",
                    ));
                }
            }
            Platform::Discord => {
                if *identity.scheme() != IdentityScheme::Snowflake
                    || identity.namespace().is_some()
                    || !identity.value().as_str().parse::<u64>().is_ok_and(|value| {
                        value != 0 && value.to_string() == identity.value().as_str()
                    })
                {
                    return Err(rejected(
                        "Discord target must be an exact snowflake identity",
                    ));
                }
            }
            _ => {
                return Err(rejected("unsupported native target platform"));
            }
        }
    }
    Ok(())
}

fn feishu_recipient(target: &DeliveryTarget) -> &IdentityRef {
    match target {
        DeliveryTarget::Actor { actor } => actor,
        DeliveryTarget::Conversation { conversation } => match conversation {
            ConversationRef::Direct {
                participant,
                conversation,
            } => conversation.as_ref().unwrap_or(participant),
            ConversationRef::Group { group } => group,
            ConversationRef::Channel { channel, .. } => channel,
            _ => unreachable!("已校验的飞书目标会话类型"),
        },
        DeliveryTarget::Channel { channel, .. } => channel,
        _ => unreachable!("已校验的飞书投递目标类型"),
    }
}
pub fn route_identity(route: &RuntimeBotRoute) -> Result<BotIdentity, DomainModelError> {
    let platform = match route.platform.as_str() {
        "onebot" | "OneBot v11" => Platform::OneBot,
        "telegram" => Platform::Telegram,
        "discord" => Platform::Discord,
        "feishu" => Platform::Feishu,
        "qq" | "qq_official" => Platform::QqOfficial,
        name => Platform::Other(name.into()),
    };
    BotIdentity::new(
        &route.instance_id,
        IdentityRef::new(
            platform,
            IdentityKind::Bot,
            IdentityScheme::Native,
            &route.bot_self_id,
            None::<String>,
        )?,
        None,
    )
}
fn id(value: &NativeId) -> Value {
    let text = value.as_str();
    match text.parse::<i64>() {
        // 旧版数值型编解码器需要 JSON 整数，但前提是转回字符串后与原值分毫不差，
        // 绝不能破坏带前导零或符号的平台原生 ID 拼写（如 `"00042"`、`"+42"` 或 `"-0"`）。
        Ok(number) if number.to_string() == text => json!(number),
        _ => json!(text),
    }
}
fn conversation_target(conversation: &ConversationRef) -> Value {
    match conversation {
        ConversationRef::Direct {
            participant,
            conversation,
        } => {
            json!({"message_type":"private","user_id":id(participant.value()),"chat_id":conversation.as_ref().map(|c|id(c.value()))})
        }
        ConversationRef::Group { group } => {
            json!({"message_type":"group","group_id":id(group.value())})
        }
        ConversationRef::Channel { channel, parent } => {
            json!({"message_type":"channel","channel_id":id(channel.value()),"guild_id":parent.as_ref().map(|p|id(p.value()))})
        }
        _ => json!({}),
    }
}
fn target(target: &DeliveryTarget) -> Value {
    match target {
        DeliveryTarget::Actor { actor } => {
            json!({"message_type":"private","user_id":id(actor.value())})
        }
        DeliveryTarget::Conversation { conversation } => conversation_target(conversation),
        DeliveryTarget::Channel { channel, parent } => {
            json!({"message_type":"channel","channel_id":id(channel.value()),"guild_id":parent.as_ref().map(|p|id(p.value()))})
        }
        _ => json!({}),
    }
}
fn encode(action: &mut OutboundAction) -> Result<Value, ActionError> {
    let (name, params) = match action {
        OutboundAction::SendMessage {
            target: destination,
            segments,
            reply_to,
        } => {
            let mut params = target(destination);
            let mut message = Vec::new();
            if let Some(reply) = reply_to {
                message.push(json!({"type":"reply","data":{"id":reply.as_str()}}));
                params["reply_to"] = id(reply);
                params["message_id"] = id(reply);
            }
            message.extend(segments.iter().map(crate::domain::projection::segment_json));
            params["message"] = Value::Array(message);
            ("send_msg".to_string(), params)
        }
        OutboundAction::DeleteMessage {
            target: destination,
            message_id,
        } => {
            let mut params = destination
                .as_ref()
                .map(target)
                .unwrap_or_else(|| json!({}));
            params["message_id"] = id(message_id);
            ("delete_msg".to_string(), params)
        }
        OutboundAction::Query { query } => match query {
            QueryOperation::GroupList => ("get_group_list".into(), json!({})),
            QueryOperation::FriendList => ("get_friend_list".into(), json!({})),
            QueryOperation::LoginInfo => ("get_login_info".into(), json!({})),
            QueryOperation::GroupMember {
                conversation,
                actor,
            } => {
                let mut params = conversation_target(conversation);
                params["user_id"] = id(actor.value());
                ("get_group_member_info".into(), params)
            }
            QueryOperation::Raw { name, params } => (std::mem::take(name), std::mem::take(params)),
            _ => {
                return Err(ActionError::new(
                    ActionErrorKind::UnsupportedCapability,
                    "不支持的查询操作类型",
                ));
            }
        },
        // 使用 `std::mem::take` 移出负载所有权而非深拷贝：原始平台动作可能携带数十 MB 的内联 Base64 媒体数据，
        // 而编码后的 JSON 是该负载的唯一消费者。原 `params` 槽位仅保留极小的接收方路由元数据，
        // 供编码之后执行的出站审计阶段提取消息发送记录。
        OutboundAction::RawPlatform {
            action: name,
            params,
        } => {
            let payload = std::mem::take(params);
            *params = audit_routing_metadata(&payload);
            (name.clone(), payload)
        }
        // 现有的 `RuntimeActionSink` 编解码器尚未支持其余强类型动作；直接明确拒绝，绝不篡改动作语义。
        _ => {
            return Err(ActionError::new(
                ActionErrorKind::UnsupportedCapability,
                "operation has no platform sink codec",
            ));
        }
    };
    let mut value = serde_json::Map::with_capacity(2);
    value.insert("action".into(), Value::String(name));
    value.insert("params".into(), params);
    Ok(Value::Object(value))
}
fn error(error: crate::error::NuonoError) -> ActionError {
    ActionError::new(ActionErrorKind::TransientTransport, error.to_string())
}

/// 提取出站审计阶段从 `RawPlatform` 动作中所需的最小接收方路由元数据。
///
/// 严禁在此列表中加入 `message` 等正文字段，否则会把上面通过移动语义省下的大对象深拷贝重新引回来。
fn audit_routing_metadata(params: &Value) -> Value {
    const KEYS: [&str; 9] = [
        "message_type",
        "group_id",
        "group_openid",
        "user_id",
        "openid",
        "channel_id",
        "chat_id",
        "guild_id",
        "qq_target_type",
    ];
    let mut metadata = serde_json::Map::new();
    if let Some(params) = params.as_object() {
        for key in KEYS {
            if let Some(value) = params.get(key) {
                metadata.insert(key.to_string(), value.clone());
            }
        }
    }
    Value::Object(metadata)
}
#[async_trait::async_trait]
impl ActionTransport for SinkTransport {
    fn connection_id(&self) -> &ConnectionId {
        &self.id
    }
    fn protocol(&self) -> &ProtocolDescriptor {
        &self.protocol
    }
    fn capabilities(&self) -> CapabilitySet {
        let mut caps = CapabilitySet::new();
        if matches!(
            self.bot.platform(),
            Platform::Feishu | Platform::Telegram | Platform::Discord
        ) {
            caps.set(Capability::SendFile, CapabilitySupport::Native);
        }
        caps.set(Capability::SendMessage, CapabilitySupport::Native);
        if !matches!(self.bot.platform(), Platform::Feishu | Platform::Other(_)) {
            caps.set(Capability::DeleteMessage, CapabilitySupport::Native);
        }
        match self.bot.platform() {
            Platform::OneBot => caps.set(Capability::Query, CapabilitySupport::Native),
            Platform::QqOfficial => caps.set(Capability::Query, CapabilitySupport::Degraded),
            _ => {}
        }
        caps.set(Capability::Media, CapabilitySupport::Degraded);
        let rich = nuono_app::rich_message::capabilities(self.bot.platform().as_str());
        for (capability, supported) in [
            (Capability::Markdown, rich.markdown),
            (Capability::TemplateMarkdown, rich.template_markdown),
            (Capability::Buttons, rich.buttons),
            (Capability::Interactions, rich.callbacks),
        ] {
            if supported {
                caps.set(capability, CapabilitySupport::Native);
            }
        }
        caps
    }
    async fn notify(&self, mut action: TransportAction) -> Result<(), ActionError> {
        if matches!(action.action, OutboundAction::SendFile { .. }) {
            return Err(rejected("file delivery requires a receipt-bearing call"));
        }
        let mut encoded = self.encode(&mut action).await?;
        let degradations = take_rich_degradations(&mut encoded);
        if !degradations.is_empty() {
            tracing::info!(platform=%self.route.platform,?degradations,"富消息按能力降级发送");
        }
        nuono_app::action::execute_authorized(
            &action.authority,
            &action,
            self.audit.as_deref(),
            async { self.sink.send_action(encoded).await.map_err(error) },
        )
        .await
    }
    async fn call(&self, mut action: TransportAction) -> Result<ActionReceipt, ActionError> {
        if matches!(action.action, OutboundAction::SendFile { .. }) {
            let request = self.native_file_request(&action).await?;
            return nuono_app::action::execute_authorized(
                &action.authority,
                &action,
                self.audit.as_deref(),
                async {
                    let response = self.sink.call_native_file(request).await.map_err(error)?;
                    response.validate().map_err(|error| {
                        ActionError::new(ActionErrorKind::MalformedResponse, error.to_string())
                    })?;
                    match response {
                        nuono_app::media::FileSendResponse::Unsupported {} => {
                            Err(ActionError::new(
                                ActionErrorKind::UnsupportedCapability,
                                "native file delivery is unsupported",
                            ))
                        }
                        nuono_app::media::FileSendResponse::Sent { message_ids, raw } => {
                            let ids = message_ids
                                .iter()
                                .map(|id| NativeId::new(id.clone()))
                                .collect::<Result<Vec<_>, _>>()
                                .map_err(rejected_domain)?;
                            Ok(ActionReceipt {
                                correlation_id: NativeId::new(uuid::Uuid::new_v4().to_string())
                                    .expect("UUID"),
                                platform: self.bot.platform().clone(),
                                status: ActionReceiptStatus::Completed,
                                message_ids: ids,
                                degradations: Vec::new(),
                                raw: json!({"status":"sent","message_ids":message_ids,"raw":raw}),
                            })
                        }
                    }
                },
            )
            .await;
        }
        let mut encoded = self.encode(&mut action).await?;
        let mut transport_degradations = take_rich_degradations(&mut encoded);
        let raw = nuono_app::action::execute_authorized(
            &action.authority,
            &action,
            self.audit.as_deref(),
            async { self.sink.call_action(encoded).await.map_err(error) },
        )
        .await?;
        let receipt = super::RuntimeActionReceipt::from_value(raw.clone());
        transport_degradations.extend(receipt.degradations);
        let mut degradations = transport_degradations;
        let message_ids = receipt
            .message_ids
            .into_iter()
            .filter_map(|id| match NativeId::new(id) {
                Ok(id) => Some(id),
                Err(error) => {
                    degradations.push(format!("Platform returned an invalid message ID: {error}"));
                    None
                }
            })
            .collect();
        Ok(ActionReceipt {
            correlation_id: NativeId::new(uuid::Uuid::new_v4().to_string()).expect("UUID"),
            platform: self.bot.platform().clone(),
            status: ActionReceiptStatus::Completed,
            message_ids,
            degradations,
            raw,
        })
    }
    async fn call_raw(
        &self,
        action: &str,
        params: Value,
        bot: Option<&BotIdentity>,
    ) -> Result<Value, ActionError> {
        if bot.is_some_and(|bot| bot != &self.bot) {
            return Err(ActionError::new(
                ActionErrorKind::Rejected,
                "transport bot identity mismatch",
            ));
        }
        self.call(TransportAction::new(
            self.bot.clone(),
            OutboundAction::RawPlatform {
                action: action.into(),
                params,
            },
        ))
        .await
        .map(|receipt| receipt.raw)
    }
    async fn close(&self) {} // 底层平台连接的生命周期由连接管理器持有，本借用型包装器无需执行关闭操作。
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<Value>>);

    #[async_trait::async_trait]
    impl RuntimeActionSink for RecordingSink {
        async fn send_action(&self, action: Value) -> crate::error::Result<()> {
            self.0.lock().unwrap().push(action);
            Ok(())
        }

        async fn call_action(&self, action: Value) -> crate::error::Result<Value> {
            self.0.lock().unwrap().push(action);
            Ok(json!({"message_id": "000900"}))
        }
    }

    fn identity(platform: Platform, kind: IdentityKind, value: &str) -> IdentityRef {
        let (scheme, namespace) = if platform == Platform::QqOfficial {
            (IdentityScheme::OpenId, Some("000700"))
        } else {
            (IdentityScheme::Native, None)
        };
        IdentityRef::new(platform, kind, scheme, value, namespace).unwrap()
    }

    fn onebot_identity(kind: IdentityKind, value: &str) -> IdentityRef {
        identity(Platform::OneBot, kind, value)
    }

    #[tokio::test]
    async fn typed_sends_replies_queries_and_deletes_preserve_exact_native_ids() {
        let sink = Arc::new(RecordingSink::default());
        let route = RuntimeBotRoute::new("onebot", "instance", "000700");
        let transport = SinkTransport::new(&route, 1, sink.clone()).unwrap();
        let bot = route_identity(&route).unwrap();
        let actor = onebot_identity(IdentityKind::User, "00042");
        let group = ConversationRef::group(onebot_identity(IdentityKind::Group, "00170")).unwrap();
        let channel = onebot_identity(IdentityKind::Channel, "00500");
        let parent = onebot_identity(IdentityKind::Group, "+00600");
        let targets = [
            (
                DeliveryTarget::actor(actor.clone()).unwrap(),
                json!({"user_id":"00042"}),
            ),
            (
                DeliveryTarget::conversation(
                    ConversationRef::direct(
                        actor.clone(),
                        Some(onebot_identity(IdentityKind::Conversation, "00321")),
                    )
                    .unwrap(),
                )
                .unwrap(),
                json!({"user_id":"00042","chat_id":"00321"}),
            ),
            (
                DeliveryTarget::conversation(group.clone()).unwrap(),
                json!({"group_id":"00170"}),
            ),
            (
                DeliveryTarget::conversation(
                    ConversationRef::channel(channel.clone(), Some(parent.clone())).unwrap(),
                )
                .unwrap(),
                json!({"channel_id":"00500","guild_id":"+00600"}),
            ),
            (
                DeliveryTarget::channel(channel, Some(parent)).unwrap(),
                json!({"channel_id":"00500","guild_id":"+00600"}),
            ),
        ];
        for (target, expected) in targets {
            let receipt = nuono_app::action::call_action(
                &transport,
                TransportAction::new(
                    bot.clone(),
                    OutboundAction::SendMessage {
                        target,
                        segments: vec![MessageSegment::Reply {
                            message_id: "00011".into(),
                        }],
                        reply_to: Some(NativeId::new("00099").unwrap()),
                    },
                ),
            )
            .await
            .unwrap();
            assert_eq!(receipt.message_ids[0].as_str(), "000900");
            let calls = sink.0.lock().unwrap();
            let sent = calls.last().unwrap();
            assert_eq!(sent["action"], "send_msg");
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(sent["params"][key], *value);
            }
            assert_eq!(sent["params"]["reply_to"], "00099");
            assert_eq!(sent["params"]["message_id"], "00099");
            assert_eq!(sent["params"]["message"][0]["data"]["id"], "00099");
            assert_eq!(sent["params"]["message"][1]["data"]["id"], "00011");
        }
        nuono_app::action::call_action(
            &transport,
            TransportAction::new(
                bot.clone(),
                OutboundAction::Query {
                    query: QueryOperation::GroupMember {
                        conversation: Box::new(group.clone()),
                        actor,
                    },
                },
            ),
        )
        .await
        .unwrap();
        nuono_app::action::notify_action(
            &transport,
            TransportAction::new(
                bot,
                OutboundAction::DeleteMessage {
                    target: Some(DeliveryTarget::conversation(group).unwrap()),
                    message_id: NativeId::new("00099").unwrap(),
                },
            ),
        )
        .await
        .unwrap();
        let calls = sink.0.lock().unwrap();
        assert_eq!(calls.len(), 7);
        assert_eq!(calls[5]["action"], "get_group_member_info");
        assert_eq!(calls[5]["params"]["group_id"], "00170");
        assert_eq!(calls[5]["params"]["user_id"], "00042");
        assert_eq!(calls[6]["action"], "delete_msg");
        assert_eq!(calls[6]["params"]["group_id"], "00170");
        assert_eq!(calls[6]["params"]["message_id"], "00099");
    }

    #[tokio::test]
    async fn typed_qq_sends_and_recalls_keep_native_string_ids_but_raw_is_unchanged() {
        let sink = Arc::new(RecordingSink::default());
        let route = RuntimeBotRoute::new("qq_official", "instance", "000700");
        let transport = SinkTransport::new(&route, 1, sink.clone()).unwrap();
        for value in ["42", "00042", "+42", "-0", "18446744073709551615"] {
            let targets = [
                (
                    DeliveryTarget::actor(identity(
                        Platform::QqOfficial,
                        IdentityKind::User,
                        value,
                    ))
                    .unwrap(),
                    "openid",
                    "user_id",
                    "c2c",
                ),
                (
                    DeliveryTarget::conversation(
                        ConversationRef::group(identity(
                            Platform::QqOfficial,
                            IdentityKind::Group,
                            value,
                        ))
                        .unwrap(),
                    )
                    .unwrap(),
                    "group_openid",
                    "group_id",
                    "group",
                ),
            ];
            for (target, openid, target_id, kind) in targets {
                nuono_app::action::notify_action(
                    &transport,
                    TransportAction::new(
                        route_identity(&route).unwrap(),
                        OutboundAction::SendMessage {
                            target: target.clone(),
                            segments: vec![MessageSegment::Text {
                                text: "test".into(),
                            }],
                            reply_to: Some(NativeId::new(value).unwrap()),
                        },
                    ),
                )
                .await
                .unwrap();
                {
                    let calls = sink.0.lock().unwrap();
                    let sent = calls.last().unwrap();
                    let params = &sent["params"];
                    assert_eq!(sent["action"], "send_msg");
                    assert_eq!(params[openid], value);
                    assert_eq!(params[target_id], value);
                    assert_eq!(params["qq_target_type"], kind);
                    assert_eq!(params["message_id"], value);
                    assert_eq!(params["reply_to"], value);
                    assert_eq!(params["message"][0]["data"]["id"], value);
                }
                nuono_app::action::call_action(
                    &transport,
                    TransportAction::new(
                        route_identity(&route).unwrap(),
                        OutboundAction::DeleteMessage {
                            target: Some(target),
                            message_id: NativeId::new(value).unwrap(),
                        },
                    ),
                )
                .await
                .unwrap();
                let calls = sink.0.lock().unwrap();
                let sent = calls.last().unwrap();
                let params = &sent["params"];
                assert_eq!(sent["action"], "delete_msg");
                assert_eq!(params[openid], value);
                assert_eq!(params[target_id], value);
                assert_eq!(params["qq_target_type"], kind);
                assert_eq!(params["message_id"], value);
            }
        }
        let raw = json!({"message_type":"private","openid":42,"user_id":42,"message_id":42});
        transport
            .call_raw(
                "send_msg",
                raw.clone(),
                Some(&route_identity(&route).unwrap()),
            )
            .await
            .unwrap();
        let calls = sink.0.lock().unwrap();
        assert_eq!(calls.last().unwrap()["params"], raw);
    }

    #[tokio::test]
    async fn typed_canonical_numeric_ids_keep_legacy_number_encoding() {
        let sink = Arc::new(RecordingSink::default());
        let route = RuntimeBotRoute::new("onebot", "instance", "700");
        let transport = SinkTransport::new(&route, 1, sink.clone()).unwrap();
        for value in [0_i64, 42, -1001234567890, i64::MIN, i64::MAX] {
            nuono_app::action::notify_action(
                &transport,
                TransportAction::new(
                    route_identity(&route).unwrap(),
                    OutboundAction::SendMessage {
                        target: DeliveryTarget::actor(onebot_identity(
                            IdentityKind::User,
                            &value.to_string(),
                        ))
                        .unwrap(),
                        segments: vec![MessageSegment::Text {
                            text: "test".into(),
                        }],
                        reply_to: Some(NativeId::new("99").unwrap()),
                    },
                ),
            )
            .await
            .unwrap();
            let calls = sink.0.lock().unwrap();
            let params = &calls.last().unwrap()["params"];
            assert_eq!(params["user_id"], value);
            assert_eq!(params["reply_to"], 99);
            assert_eq!(params["message_id"], 99);
        }
    }

    #[tokio::test]
    async fn raw_platform_encoding_moves_the_body_but_keeps_audit_recipients() {
        let sink = Arc::new(RecordingSink::default());
        let route = RuntimeBotRoute::new("onebot", "instance", "000700");
        let transport = SinkTransport::new(&route, 1, sink.clone()).unwrap();
        let file = format!("base64://{}", "A".repeat(1024));
        let mut action = TransportAction::new(
            route_identity(&route).unwrap(),
            OutboundAction::RawPlatform {
                action: "send_group_msg".into(),
                params: json!({
                    "group_id": "00170",
                    "message": [{"type": "video", "data": {"file": file}}],
                }),
            },
        );
        let encoded = transport.encode(&mut action).await.unwrap();
        assert_eq!(encoded["params"]["group_id"], "00170");
        assert_eq!(encoded["params"]["message"][0]["data"]["file"], file);

        // 审计阶段在编码完成之后运行，因此它依赖的接收方路由元数据必须保留在原结构体中，
        // 但沉重的消息正文绝不能被重新深拷贝回原动作结构里。
        match &action.action {
            OutboundAction::RawPlatform { params, .. } => {
                assert_eq!(params["group_id"], "00170");
                assert!(params.get("message").is_none());
                assert!(params.as_object().unwrap().len() < 3);
            }
            other => panic!("expected a raw platform action, got {other:?}"),
        }
    }
}
