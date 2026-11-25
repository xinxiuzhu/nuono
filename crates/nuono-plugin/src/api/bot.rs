use crate::api::waiter::WaitManager;
use crate::core::event_bus::EventBus;
use crate::core::event_kernel::RuntimeActionSink;
use crate::core::onebot_client::ApiClient;
use crate::core::{
    HostLogLevel, HostLogRecord, RuntimeActionReceipt, RuntimeActionRouter, RuntimeBotRoute,
};
use crate::error::{NuonoError, Result};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
#[path = "bot_rich.rs"]
mod rich;

/// 消息上下文 — 协议无关的目标信息
#[derive(Debug, Clone)]
pub struct MessageContext {
    pub self_id: i64,
    pub user_id: i64,
    pub group_id: Option<i64>,
    pub channel_id: Option<String>,
    pub message_id: String,
    pub raw_message: String,
    pub message_type: String,
    /// 平台原生字符串目标和被动回复字段。
    pub reply_context: Value,
}

impl MessageContext {
    fn json_id_to_string(v: Option<&Value>) -> Option<String> {
        v.and_then(|id| {
            id.as_str()
                .map(str::to_string)
                .or_else(|| id.as_i64().map(|id| id.to_string()))
                .or_else(|| id.as_u64().map(|id| id.to_string()))
        })
    }

    /// 从原始 JSON 构造（OneBot 格式）
    pub fn from_onebot_json(v: &Value) -> Option<Self> {
        let post_type = v.get("post_type").and_then(|p| p.as_str())?;
        if post_type != "message" {
            return None;
        }
        let mut reply_context = serde_json::Map::new();
        for key in [
            "route",
            "user_id",
            "group_id",
            "channel_id",
            "guild_id",
            "chat_id",
            "openid",
            "group_openid",
            "event_id",
            "qq_target_type",
        ] {
            if let Some(value) = v.get(key) {
                reply_context.insert(key.to_string(), value.clone());
            }
        }
        if let Some(context) = v.get("reply_context").and_then(Value::as_object) {
            reply_context.extend(context.clone());
        }
        Some(Self {
            self_id: v.get("self_id").and_then(|id| id.as_i64()).unwrap_or(0),
            user_id: v.get("user_id").and_then(|id| id.as_i64()).unwrap_or(0),
            group_id: v.get("group_id").and_then(|id| id.as_i64()),
            channel_id: Self::json_id_to_string(v.get("channel_id")),
            message_id: Self::json_id_to_string(v.get("message_id")).unwrap_or_default(),
            raw_message: v
                .get("raw_message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
            message_type: v
                .get("message_type")
                .and_then(|m| m.as_str())
                .unwrap_or("private")
                .to_string(),
            reply_context: Value::Object(reply_context),
        })
    }

    /// 从 onebot::Message 转换（兼容旧触发器系统）
    pub fn from_onebot_message(msg: &crate::onebot::Message) -> Self {
        match msg {
            crate::onebot::Message::GroupMessage(gm) => Self {
                self_id: gm.self_id,
                user_id: gm.user_id,
                group_id: Some(gm.group_id),
                channel_id: None,
                message_id: gm.message_id.to_string(),
                raw_message: gm.raw_message.clone(),
                message_type: "group".into(),
                reply_context: Value::Null,
            },
            crate::onebot::Message::PrivateMessage(pm) => Self {
                self_id: pm.self_id,
                user_id: pm.user_id,
                group_id: None,
                channel_id: None,
                message_id: pm.message_id.to_string(),
                raw_message: pm.raw_message.clone(),
                message_type: "private".into(),
                reply_context: Value::Null,
            },
        }
    }
}

/// Bot API 实例
#[derive(Clone)]
pub struct Bot {
    pub self_id: i64,
    pub event_bus: Arc<EventBus>,
    pub wait_manager: Arc<WaitManager>,
    pub api_client: Arc<ApiClient>,
    pub msg_tx: Option<tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>>,
    pub action_sink: Option<Arc<dyn RuntimeActionSink>>,
    pub action_router: Option<Arc<RuntimeActionRouter>>,
    /// 当前消息上下文
    pub current_message: Option<MessageContext>,
    wait_owner: Option<String>,
    wait_permission: Option<crate::core::command::PermissionLevel>,
}

impl std::fmt::Debug for Bot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bot")
            .field("self_id", &self.self_id)
            .field("has_msg_tx", &self.msg_tx.is_some())
            .field("has_action_sink", &self.action_sink.is_some())
            .field("has_action_router", &self.action_router.is_some())
            .field("current_message", &self.current_message)
            .field("wait_owner", &self.wait_owner)
            .field("wait_permission", &self.wait_permission)
            .finish_non_exhaustive()
    }
}

impl Bot {
    /// 回复生成的图片，不需要插件作者手工编码 base64 或拼平台上传参数。
    pub async fn send_image(&self, image: impl Into<crate::ImageSource>) -> Result<()> {
        let segment = image
            .into()
            .into_segment()
            .await
            .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
        self.send(vec![segment]).await
    }

    /// 给指定会话主动发图，沿用普通主动发送的路由和回执。
    pub async fn send_image_routed(
        &self,
        target: &crate::RuntimeMessageTarget,
        image: impl Into<crate::ImageSource>,
    ) -> Result<RuntimeActionReceipt> {
        let segment = image
            .into()
            .into_segment()
            .await
            .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
        self.send_routed(target, vec![segment]).await
    }
    /// 创建新的 Bot 实例
    pub fn new(
        self_id: i64,
        event_bus: Arc<EventBus>,
        wait_manager: Arc<WaitManager>,
        api_client: Arc<ApiClient>,
        msg_tx: Option<tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>>,
        action_sink: Option<Arc<dyn RuntimeActionSink>>,
    ) -> Self {
        Self {
            self_id,
            event_bus,
            wait_manager,
            api_client,
            msg_tx,
            action_sink,
            action_router: None,
            current_message: None,
            wait_owner: None,
            wait_permission: None,
        }
    }

    /// 附加进程级共享的精确字符串动作路由器，供插件跨平台主动调用 API。
    pub fn with_action_router(mut self, action_router: Arc<RuntimeActionRouter>) -> Self {
        self.action_router = Some(action_router);
        self
    }

    /// 设置由宿主注入的交互等待归属插件与权限要求（克隆给回调处理函数的 `Bot` 会继承此配置）。
    pub fn with_wait_owner(
        mut self,
        plugin: impl Into<String>,
        permission: Option<crate::core::command::PermissionLevel>,
    ) -> Self {
        self.wait_owner = Some(plugin.into());
        self.wait_permission = permission;
        self
    }

    /// 通过宿主的日志订阅器输出插件结构化日志。
    ///
    /// `fields` 通常应为 JSON 对象；若传入其他 JSON 类型也会原样记录，
    /// 确保插件日志绝不会因为字段结构不符而丢失或报错。
    pub fn host_log(
        &self,
        level: HostLogLevel,
        target: impl Into<String>,
        message: impl Into<String>,
        fields: Value,
    ) -> Result<()> {
        let router = self
            .action_router
            .as_ref()
            .ok_or(NuonoError::MessageChannelNotSet)?;
        router.log(
            HostLogRecord::new(level, target, message)
                .with_plugin(self.wait_owner.clone())
                .with_fields(fields),
        );
        Ok(())
    }

    pub fn host_trace(
        &self,
        target: impl Into<String>,
        message: impl Into<String>,
        fields: Value,
    ) -> Result<()> {
        self.host_log(HostLogLevel::Trace, target, message, fields)
    }

    pub fn host_debug(
        &self,
        target: impl Into<String>,
        message: impl Into<String>,
        fields: Value,
    ) -> Result<()> {
        self.host_log(HostLogLevel::Debug, target, message, fields)
    }

    pub fn host_info(
        &self,
        target: impl Into<String>,
        message: impl Into<String>,
        fields: Value,
    ) -> Result<()> {
        self.host_log(HostLogLevel::Info, target, message, fields)
    }

    pub fn host_warn(
        &self,
        target: impl Into<String>,
        message: impl Into<String>,
        fields: Value,
    ) -> Result<()> {
        self.host_log(HostLogLevel::Warn, target, message, fields)
    }

    pub fn host_error(
        &self,
        target: impl Into<String>,
        message: impl Into<String>,
        fields: Value,
    ) -> Result<()> {
        self.host_log(HostLogLevel::Error, target, message, fields)
    }

    /// 返回当前已连接到宿主运行时的所有活跃 Bot 路由。
    pub fn connected_routes(&self) -> Result<Vec<RuntimeBotRoute>> {
        let router = self
            .action_router
            .as_ref()
            .ok_or(NuonoError::MessageChannelNotSet)?;
        Ok(router.connected_routes())
    }

    /// 按精确的平台原生 Bot 身份字符串路由一次“只发不取回执”（fire-and-forget）的平台动作。
    pub async fn notify_routed(&self, route: &RuntimeBotRoute, action: Value) -> Result<()> {
        let router = self
            .action_router
            .as_ref()
            .ok_or(NuonoError::MessageChannelNotSet)?;
        crate::core::action_log::scope(self.wait_owner.clone(), router.notify(route, action)).await
    }

    /// 按指定路由调用平台动作并返回适配器响应的原始 JSON。
    pub async fn call_routed(&self, route: &RuntimeBotRoute, action: Value) -> Result<Value> {
        let router = self
            .action_router
            .as_ref()
            .ok_or(NuonoError::MessageChannelNotSet)?;
        crate::core::action_log::scope(self.wait_owner.clone(), router.call(route, action)).await
    }

    /// 按指定路由调用平台动作，提取精确的平台原生消息 ID 列表并保留原始回包数据。
    pub async fn call_routed_receipt(
        &self,
        route: &RuntimeBotRoute,
        action: Value,
    ) -> Result<RuntimeActionReceipt> {
        let router = self
            .action_router
            .as_ref()
            .ok_or(NuonoError::MessageChannelNotSet)?;
        crate::core::action_log::scope(self.wait_owner.clone(), router.call_receipt(route, action))
            .await
    }

    /// 主动跨平台发送消息：不依赖当前消息上下文、不使用数字 ID、不让插件拼
    /// 适配器特定操作。
    ///
    /// 框架依据 `target.route.platform` 生成各 sink 能解析的规范 `send_msg` action，
    /// 走 [`Self::call_routed_receipt`] 路由到注册的 bot 实例。离线路由返回
    /// [`NuonoError::Connection`]；成功时 receipt 保留精确的字符串 message IDs 与
    /// 原始回包。
    pub async fn send_routed(
        &self,
        target: &crate::api::routed::RuntimeMessageTarget,
        message: impl Into<SendableMessage>,
    ) -> Result<RuntimeActionReceipt> {
        target.validate()?;
        let (message, degradations) = self.prepare_rich(Some(target), message.into()).await?;
        let action = crate::api::routed::build_send_msg_action_owned(
            &target.route.platform,
            target,
            message,
        )?;
        let mut receipt = self.call_routed_receipt(&target.route, action).await?;
        receipt.degradations.extend(degradations);
        Ok(receipt)
    }

    /// 主动发送消息但不等待平台回执：适合 OneBot 等只确认投递、未必返回
    /// `call_action` receipt 的适配器。发送动作仍由框架统一构造，不让插件拼平台字段。
    pub async fn send_routed_notify(
        &self,
        target: &crate::api::routed::RuntimeMessageTarget,
        message: impl Into<SendableMessage>,
    ) -> Result<()> {
        target.validate()?;
        let (message, _) = self.prepare_rich(Some(target), message.into()).await?;
        let action = crate::api::routed::build_send_msg_action_owned(
            &target.route.platform,
            target,
            message,
        )?;
        self.notify_routed(&target.route, action).await
    }

    /// 执行强类型领域动作，完整保留身份体系（scheme）与命名空间（namespace）。
    /// 由精确路由定位当前已注册的连接；鉴权、审计、适配器作用域校验和超时取消均走标准强类型动作通道。
    pub async fn call_native_routed(
        &self,
        route: &RuntimeBotRoute,
        action: crate::domain::OutboundAction,
    ) -> Result<RuntimeActionReceipt> {
        let (action, degradations) = self.prepare_native_rich(route, action).await?;
        let router = self
            .action_router
            .as_ref()
            .ok_or(NuonoError::MessageChannelNotSet)?;
        let bot = crate::core::runtime_transport::route_identity(route)
            .map_err(|error| NuonoError::Validation(error.to_string()))?;
        let mut receipt = crate::core::action_log::scope(
            self.wait_owner.clone(),
            router.call_typed_routed(
                route,
                nuono_app::transport::TransportAction::new(bot, action),
            ),
        )
        .await?;
        receipt.degradations.extend(degradations);
        Ok(receipt)
    }

    /// 发送无需等待回执的强类型原生领域动作。
    pub async fn notify_native_routed(
        &self,
        route: &RuntimeBotRoute,
        action: crate::domain::OutboundAction,
    ) -> Result<()> {
        let (action, _) = self.prepare_native_rich(route, action).await?;
        let router = self
            .action_router
            .as_ref()
            .ok_or(NuonoError::MessageChannelNotSet)?;
        let bot = crate::core::runtime_transport::route_identity(route)
            .map_err(|error| NuonoError::Validation(error.to_string()))?;
        crate::core::action_log::scope(
            self.wait_owner.clone(),
            router.notify_typed_routed(
                route,
                nuono_app::transport::TransportAction::new(bot, action),
            ),
        )
        .await
    }

    pub async fn send_native_routed(
        &self,
        route: &RuntimeBotRoute,
        target: &crate::domain::DeliveryTarget,
        message: impl Into<NativeMessage>,
    ) -> Result<RuntimeActionReceipt> {
        self.call_native_routed(
            route,
            crate::domain::OutboundAction::SendMessage {
                target: target.clone(),
                segments: message.into().into_segments(),
                reply_to: None,
            },
        )
        .await
    }

    pub async fn send_native_routed_notify(
        &self,
        route: &RuntimeBotRoute,
        target: &crate::domain::DeliveryTarget,
        message: impl Into<NativeMessage>,
    ) -> Result<()> {
        self.notify_native_routed(
            route,
            crate::domain::OutboundAction::SendMessage {
                target: target.clone(),
                segments: message.into().into_segments(),
                reply_to: None,
            },
        )
        .await
    }

    /// 发送消息（回复原消息源）
    pub async fn send(&self, message: impl Into<SendableMessage>) -> Result<()> {
        let (msg, _) = self.prepare_rich(None, message.into()).await?;
        let api_call_json = self.build_reply_api_call(msg)?;

        if let Some(tx) = &self.msg_tx {
            if let Err(e) = tx
                .send(nuono_app::transport::TransportFrame::Text(
                    api_call_json.clone(),
                ))
                .await
            {
                tracing::error!(
                    error = %e,
                    bot_id = %self.self_id,
                    "message send failed"
                );
                return Err(NuonoError::MessageSend(e.to_string()));
            }
            tracing::debug!(
                bot_id = %self.self_id,
                action = "send_message",
                "message sent"
            );
        } else {
            self.send_action_json(&api_call_json).await?;
        }

        Ok(())
    }

    async fn send_action_json(&self, api_call_json: &str) -> Result<()> {
        let Some(action_sink) = &self.action_sink else {
            tracing::warn!(
                bot_id = %self.self_id,
                "message channel not set"
            );
            return Err(NuonoError::MessageChannelNotSet);
        };
        let action: Value = serde_json::from_str(api_call_json)?;
        crate::core::action_log::scope(self.wait_owner.clone(), action_sink.send_action(action))
            .await
    }

    async fn send_action_value(&self, action: &str, params: Value) -> Result<()> {
        let Some(action_sink) = &self.action_sink else {
            return Err(NuonoError::MessageChannelNotSet);
        };
        crate::core::action_log::scope(
            self.wait_owner.clone(),
            action_sink.send_action(json!({
                "action": action,
                "params": params,
            })),
        )
        .await
    }

    async fn call_action_value(&self, action: &str, params: Value) -> Result<Value> {
        let Some(action_sink) = &self.action_sink else {
            return Err(NuonoError::MessageChannelNotSet);
        };
        crate::core::action_log::scope(
            self.wait_owner.clone(),
            action_sink.call_action(json!({
                "action": action,
                "params": params,
            })),
        )
        .await
    }

    /// 触发型 action（无需返回值）的统一出口：优先直连 WS，回退 runtime sink。
    async fn fire(&self, action: &str, params: Value) -> Result<()> {
        if let Some(tx) = &self.msg_tx {
            crate::core::action_log::scope(
                self.wait_owner.clone(),
                self.api_client.send(action, params, tx),
            )
            .await
            .map_err(|e| NuonoError::MessageSend(e.to_string()))
        } else {
            self.send_action_value(action, params).await
        }
    }

    /// 查询型 action（需要返回值）的统一出口。
    async fn query(&self, action: &str, params: Value) -> Result<Value> {
        if let Some(tx) = &self.msg_tx {
            crate::core::action_log::scope(
                self.wait_owner.clone(),
                self.api_client.call(action, params, tx),
            )
            .await
            .map_err(|e| NuonoError::MessageSend(e.to_string()))
        } else {
            self.call_action_value(action, params).await
        }
    }

    /// 列表型查询：返回值不是数组时降级为空列表，与既有列表方法一致。
    async fn query_list(&self, action: &str, params: Value) -> Result<Vec<Value>> {
        let resp = self.query(action, params).await?;
        Ok(resp.as_array().cloned().unwrap_or_default())
    }

    /// 构造回复消息的 API 调用
    fn build_reply_api_call(&self, message: SendableMessage) -> Result<String> {
        let ctx = self
            .current_message
            .as_ref()
            .ok_or(NuonoError::MessageContextNotSet)?;

        let msg = message.into_json();
        let mut params = serde_json::Map::with_capacity(6);
        let action = match ctx.message_type.as_str() {
            "group" => {
                params.insert("group_id".into(), json!(ctx.group_id.unwrap_or(0)));
                "send_group_msg"
            }
            "channel" => {
                params.insert(
                    "channel_id".into(),
                    json!(
                        ctx.channel_id
                            .clone()
                            .unwrap_or_else(|| ctx.user_id.to_string())
                    ),
                );
                "send_guild_channel_msg"
            }
            _ => {
                params.insert("user_id".into(), json!(ctx.user_id));
                "send_private_msg"
            }
        };
        params.insert("message".into(), msg);

        if let Some(channel_id) = &ctx.channel_id {
            params.insert("channel_id".into(), json!(channel_id));
        }
        if let Some(context) = ctx.reply_context.as_object() {
            for (key, value) in context {
                if key != "route" {
                    params.insert(key.clone(), value.clone());
                }
            }
        }
        params.insert("message_id".into(), json!(ctx.message_id));
        if let Some(mut context) = ctx.reply_context.as_object().cloned() {
            context.remove("route");
            params.insert("reply_context".into(), Value::Object(context));
        }

        let mut api_call = serde_json::Map::with_capacity(3);
        api_call.insert("action".into(), json!(action));
        api_call.insert("reply_current".into(), json!(true));
        api_call.insert("params".into(), Value::Object(params));

        Ok(serde_json::to_string(&Value::Object(api_call))?)
    }

    /// 设置当前消息上下文
    pub fn set_current_message(&mut self, ctx: MessageContext) {
        self.current_message = Some(ctx);
    }

    /// 清除当前消息
    pub fn clear_current_message(&mut self) {
        self.current_message = None;
    }

    /// 设置消息发送通道
    pub fn set_msg_tx(
        &mut self,
        tx: tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>,
    ) {
        self.msg_tx = Some(tx);
    }

    /// 发送到指定目标
    pub async fn send_to(
        &self,
        target: MessageTarget,
        message: impl Into<SendableMessage>,
    ) -> Result<()> {
        let rich_target = self.rich_reply_target().map(|mut route| {
            route.kind = match &target {
                MessageTarget::User(id) => crate::api::routed::RuntimeTargetKind::Private {
                    user_id: id.to_string(),
                },
                MessageTarget::Group(id) => crate::api::routed::RuntimeTargetKind::Group {
                    group_id: id.to_string(),
                },
                MessageTarget::Channel(guild, id) => {
                    crate::api::routed::RuntimeTargetKind::Channel {
                        channel_id: id.to_string(),
                        parent: Some(guild.to_string()),
                    }
                }
            };
            route
        });
        let (msg, _) = self
            .prepare_rich(rich_target.as_ref(), message.into())
            .await?;
        let msg = msg.into_json();
        let (action, params) = match target {
            MessageTarget::User(user_id) => (
                "send_private_msg",
                json!({
                    "user_id": user_id,
                    "message": msg
                }),
            ),
            MessageTarget::Group(group_id) => (
                "send_group_msg",
                json!({
                    "group_id": group_id,
                    "message": msg
                }),
            ),
            MessageTarget::Channel(guild_id, channel_id) => (
                "send_guild_channel_msg",
                json!({
                    "guild_id": guild_id,
                    "channel_id": channel_id,
                    "message": msg
                }),
            ),
        };

        if let Some(tx) = &self.msg_tx {
            self.api_client
                .send(action, params, tx)
                .await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
        } else {
            self.send_action_value(action, params).await?;
        }
        Ok(())
    }

    /// 等待用户回复
    pub async fn wait_for_reply(&self, timeout: Duration) -> Result<Value> {
        self.prepare_reply_wait(timeout, crate::api::waiter::ReplyWaitMode::Single)?
            .receive(timeout)
            .await
    }

    /// 等待回复并保留这条新消息的原生身份与语言快照；原始 JSON 不增加内部字段。
    pub async fn wait_for_reply_event(
        &self,
        timeout: Duration,
    ) -> Result<crate::api::waiter::WaitEvent> {
        self.prepare_reply_event_wait(timeout, crate::api::waiter::ReplyWaitMode::Single)?
            .receive(timeout)
            .await
    }

    /// 在发送交互提示消息前预先注册回复等待，避免因用户秒回而漏收消息。
    pub fn prepare_reply_wait(
        &self,
        timeout: Duration,
        mode: crate::api::waiter::ReplyWaitMode,
    ) -> Result<crate::api::waiter::WaitRegistration<Value>> {
        let mut waiter = self.reply_waiter()?;
        waiter.set_mode(mode);
        waiter.try_register(timeout)
    }

    /// 在发送交互提示消息前预先注册回复事件等待，保留新回复的原生领域身份与语言设置。
    /// 取消或丢弃该注册句柄只会撤销当前这一次等待，不会误伤后续新注册的等待。
    pub fn prepare_reply_event_wait(
        &self,
        timeout: Duration,
        mode: crate::api::waiter::ReplyWaitMode,
    ) -> Result<crate::api::waiter::WaitRegistration<crate::api::waiter::WaitEvent>> {
        let mut waiter = self.reply_waiter()?;
        waiter.set_mode(mode);
        waiter.try_register_event(timeout)
    }

    fn reply_waiter(&self) -> Result<crate::api::waiter::Waiter> {
        let plugin = self
            .wait_owner
            .as_ref()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| NuonoError::Other("等待回复需要宿主提供插件归属".into()))?;
        let ctx = self
            .current_message
            .as_ref()
            .ok_or(NuonoError::MessageContextNotSet)?;

        let native = ctx.reply_context.as_object();
        let user_id = native
            .and_then(|context| context.get("user_id").or_else(|| context.get("openid")))
            .and_then(|value| MessageContext::json_id_to_string(Some(value)))
            .unwrap_or_else(|| ctx.user_id.to_string());
        let group_id = native
            .and_then(|context| {
                context
                    .get("group_id")
                    .or_else(|| context.get("group_openid"))
            })
            .and_then(|value| MessageContext::json_id_to_string(Some(value)))
            .or_else(|| ctx.group_id.map(|id| id.to_string()))
            .or_else(|| {
                (ctx.message_type == "channel")
                    .then(|| ctx.channel_id.clone())
                    .flatten()
            });
        let session_id = match &group_id {
            Some(group_id) => format!("{}:{}:{}", self.self_id, group_id, user_id),
            None => format!("{}:private:{}", self.self_id, user_id),
        };

        Ok(
            crate::api::waiter::Waiter::new(session_id, self.wait_manager.clone()).with_scope(
                crate::api::waiter::WaitScope {
                    route: native
                        .and_then(|context| context.get("route"))
                        .cloned()
                        .and_then(|route| serde_json::from_value(route).ok()),
                    plugin_name: Some(plugin.clone()),
                    required_permission: self.wait_permission,
                    bot_id: self.self_id,
                    user_id,
                    group_id,
                    multiply: false,
                },
            ),
        )
    }

    /// 撤回消息
    pub async fn recall(&self, message_id: i64) -> Result<()> {
        if let Some(tx) = &self.msg_tx {
            self.api_client
                .send("delete_msg", json!({"message_id": message_id}), tx)
                .await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
        } else {
            self.send_action_value("delete_msg", json!({"message_id": message_id}))
                .await?;
        }
        Ok(())
    }

    /// 发送点赞
    pub async fn send_like(&self, user_id: i64, times: i32) -> Result<()> {
        if let Some(tx) = &self.msg_tx {
            self.api_client
                .send("send_like", json!({"user_id": user_id, "times": times}), tx)
                .await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
        } else {
            self.send_action_value("send_like", json!({"user_id": user_id, "times": times}))
                .await?;
        }
        Ok(())
    }

    /// 设置群成员名片
    pub async fn set_group_card(&self, group_id: i64, user_id: i64, card: String) -> Result<()> {
        if let Some(tx) = &self.msg_tx {
            self.api_client
                .send(
                    "set_group_card",
                    json!({"group_id": group_id, "user_id": user_id, "card": card}),
                    tx,
                )
                .await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
        } else {
            self.send_action_value(
                "set_group_card",
                json!({"group_id": group_id, "user_id": user_id, "card": card}),
            )
            .await?;
        }
        Ok(())
    }

    /// 踢出群成员
    pub async fn set_group_kick(
        &self,
        group_id: i64,
        user_id: i64,
        reject_add_request: bool,
    ) -> Result<()> {
        if let Some(tx) = &self.msg_tx {
            self.api_client.send("set_group_kick", json!({"group_id": group_id, "user_id": user_id, "reject_add_request": reject_add_request}), tx).await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
        } else {
            self.send_action_value(
                "set_group_kick",
                json!({"group_id": group_id, "user_id": user_id, "reject_add_request": reject_add_request}),
            )
            .await?;
        }
        Ok(())
    }

    /// 禁言群成员
    pub async fn set_group_ban(&self, group_id: i64, user_id: i64, duration: i32) -> Result<()> {
        if let Some(tx) = &self.msg_tx {
            self.api_client
                .send(
                    "set_group_ban",
                    json!({"group_id": group_id, "user_id": user_id, "duration": duration}),
                    tx,
                )
                .await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
        } else {
            self.send_action_value(
                "set_group_ban",
                json!({"group_id": group_id, "user_id": user_id, "duration": duration}),
            )
            .await?;
        }
        Ok(())
    }

    /// 获取群成员信息
    pub async fn get_group_member_info(&self, group_id: i64, user_id: i64) -> Result<Value> {
        if let Some(tx) = &self.msg_tx {
            let resp = self
                .api_client
                .call(
                    "get_group_member_info",
                    json!({"group_id": group_id, "user_id": user_id}),
                    tx,
                )
                .await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
            Ok(resp)
        } else {
            self.call_action_value(
                "get_group_member_info",
                json!({"group_id": group_id, "user_id": user_id}),
            )
            .await
        }
    }

    /// 获取好友列表
    pub async fn get_friend_list(&self) -> Result<Vec<Value>> {
        let resp = if let Some(tx) = &self.msg_tx {
            self.api_client
                .call("get_friend_list", json!({}), tx)
                .await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?
        } else {
            self.call_action_value("get_friend_list", json!({})).await?
        };
        if let Some(arr) = resp.as_array() {
            Ok(arr.clone())
        } else {
            Ok(vec![])
        }
    }

    /// 获取群列表
    pub async fn get_group_list(&self) -> Result<Vec<Value>> {
        let resp = if let Some(tx) = &self.msg_tx {
            self.api_client
                .call("get_group_list", json!({}), tx)
                .await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?
        } else {
            self.call_action_value("get_group_list", json!({})).await?
        };
        if let Some(arr) = resp.as_array() {
            Ok(arr.clone())
        } else {
            Ok(vec![])
        }
    }

    /// 主动发送消息到指定目标（不需要当前消息上下文）
    /// 对应 gsuid_core 的 bot.target_send
    pub async fn target_send(
        &self,
        target_type: &str,
        target_id: i64,
        message: impl Into<SendableMessage>,
    ) -> Result<()> {
        let rich_target = self.rich_reply_target().map(|mut route| {
            route.kind = if target_type == "group" {
                crate::api::routed::RuntimeTargetKind::Group {
                    group_id: target_id.to_string(),
                }
            } else {
                crate::api::routed::RuntimeTargetKind::Private {
                    user_id: target_id.to_string(),
                }
            };
            route
        });
        let (msg, _) = self
            .prepare_rich(rich_target.as_ref(), message.into())
            .await?;
        let msg = msg.into_json();
        let (action, params) = match target_type {
            "group" => (
                "send_group_msg",
                json!({ "group_id": target_id, "message": msg }),
            ),
            "user" | "private" => (
                "send_private_msg",
                json!({ "user_id": target_id, "message": msg }),
            ),
            _ => {
                return Err(NuonoError::Validation(format!(
                    "未知的目标类型: {}",
                    target_type
                )));
            }
        };

        if let Some(tx) = &self.msg_tx {
            self.api_client
                .send(action, params, tx)
                .await
                .map_err(|e| NuonoError::MessageSend(e.to_string()))?;
        } else {
            self.send_action_value(action, params).await?;
        }
        Ok(())
    }

    // ── 消息查询（标准 v11） ──

    /// 获取消息详情：发送者、时间、消息段、引用关系等。
    pub async fn get_msg(&self, message_id: i64) -> Result<Value> {
        self.query("get_msg", json!({"message_id": message_id}))
            .await
    }

    /// 展开合并转发消息的内容。
    pub async fn get_forward_msg(&self, message_id: impl Into<String>) -> Result<Value> {
        self.query("get_forward_msg", json!({"message_id": message_id.into()}))
            .await
    }

    /// 接收图片后取下载信息（file_id 来自消息里的 image 段）。
    pub async fn get_image(&self, file: impl Into<String>) -> Result<Value> {
        self.query("get_image", json!({"file": file.into()})).await
    }

    /// 接收语音后转码取文件（`out_format` 如 "mp3"、"wav"）。
    pub async fn get_record(&self, file: impl Into<String>, out_format: &str) -> Result<Value> {
        self.query(
            "get_record",
            json!({"file": file.into(), "out_format": out_format}),
        )
        .await
    }

    // ── 合并转发（go-cqhttp 扩展，NapCat 支持） ──

    /// 发送合并转发到群。`messages` 为 node 段数组，
    /// 用 [`crate::api::MessageSegment::node_with_info`] 构造。
    pub async fn send_group_forward_msg(
        &self,
        group_id: i64,
        messages: &[crate::api::MessageSegment],
    ) -> Result<()> {
        let wire: Vec<Value> = messages
            .iter()
            .map(crate::api::MessageSegment::to_json)
            .collect();
        self.fire(
            "send_group_forward_msg",
            json!({"group_id": group_id, "messages": wire}),
        )
        .await
    }

    /// 发送合并转发到私聊。
    pub async fn send_private_forward_msg(
        &self,
        user_id: i64,
        messages: &[crate::api::MessageSegment],
    ) -> Result<()> {
        let wire: Vec<Value> = messages
            .iter()
            .map(crate::api::MessageSegment::to_json)
            .collect();
        self.fire(
            "send_private_forward_msg",
            json!({"user_id": user_id, "messages": wire}),
        )
        .await
    }

    /// 单条转发某消息到好友（NapCat 扩展）。
    pub async fn forward_friend_single_msg(&self, user_id: i64, message_id: i64) -> Result<()> {
        self.fire(
            "forward_friend_single_msg",
            json!({"user_id": user_id, "message_id": message_id}),
        )
        .await
    }

    /// 单条转发某消息到群（NapCat 扩展）。
    pub async fn forward_group_single_msg(&self, group_id: i64, message_id: i64) -> Result<()> {
        self.fire(
            "forward_group_single_msg",
            json!({"group_id": group_id, "message_id": message_id}),
        )
        .await
    }

    /// 好友历史消息（NapCat 扩展）。`message_seq` 为起始序号，`None` 从最新开始。
    pub async fn get_friend_msg_history(
        &self,
        user_id: i64,
        message_seq: Option<i64>,
        count: i64,
    ) -> Result<Value> {
        self.query(
            "get_friend_msg_history",
            json!({"user_id": user_id, "message_seq": message_seq.unwrap_or(0), "count": count}),
        )
        .await
    }

    /// 群历史消息（NapCat 扩展）。
    pub async fn get_group_msg_history(
        &self,
        group_id: i64,
        message_seq: Option<i64>,
        count: i64,
    ) -> Result<Value> {
        self.query(
            "get_group_msg_history",
            json!({"group_id": group_id, "message_seq": message_seq.unwrap_or(0), "count": count}),
        )
        .await
    }

    /// 最近会话列表（NapCat 扩展）。
    pub async fn get_recent_contact(&self, count: i64) -> Result<Vec<Value>> {
        self.query_list("get_recent_contact", json!({"count": count}))
            .await
    }

    // ── 表情回应与已读（NapCat 扩展） ──

    /// 给消息添加表情回应。`emoji_id` 如 "76"（赞）。
    pub async fn set_msg_emoji_like(&self, message_id: i64, emoji_id: &str) -> Result<()> {
        self.fire(
            "set_msg_emoji_like",
            json!({"message_id": message_id, "emoji_id": emoji_id}),
        )
        .await
    }

    /// 标记群消息已读（消除群聊未读红点）。
    pub async fn mark_group_msg_as_read(&self, group_id: i64) -> Result<()> {
        self.fire("mark_group_msg_as_read", json!({"group_id": group_id}))
            .await
    }

    /// 标记私聊已读。
    pub async fn mark_private_msg_as_read(&self, user_id: i64) -> Result<()> {
        self.fire("mark_private_msg_as_read", json!({"user_id": user_id}))
            .await
    }

    /// 全部标记已读。
    pub async fn mark_all_as_read(&self) -> Result<()> {
        self.fire("_mark_all_as_read", json!({})).await
    }

    // ── 戳一戳（NapCat 扩展） ──

    /// 戳好友。
    pub async fn friend_poke(&self, user_id: i64) -> Result<()> {
        self.fire("friend_poke", json!({"user_id": user_id})).await
    }

    /// 在群里戳某人。
    pub async fn group_poke(&self, group_id: i64, user_id: i64) -> Result<()> {
        self.fire(
            "group_poke",
            json!({"group_id": group_id, "user_id": user_id}),
        )
        .await
    }

    // ── 精华消息（go-cqhttp 扩展，NapCat 支持） ──

    /// 设置精华消息。
    pub async fn set_essence_msg(&self, message_id: i64) -> Result<()> {
        self.fire("set_essence_msg", json!({"message_id": message_id}))
            .await
    }

    /// 移除精华消息。
    pub async fn delete_essence_msg(&self, message_id: i64) -> Result<()> {
        self.fire("delete_essence_msg", json!({"message_id": message_id}))
            .await
    }

    /// 获取群精华消息列表。
    pub async fn get_essence_msg_list(&self, group_id: i64) -> Result<Vec<Value>> {
        self.query_list("get_essence_msg_list", json!({"group_id": group_id}))
            .await
    }

    // ── 文件（NapCat / go-cqhttp 扩展） ──

    /// 按 file_id 取文件本地路径或下载 URL。收文件消息后用。
    pub async fn get_file(&self, file_id: impl Into<String>) -> Result<Value> {
        self.query("get_file", json!({"file_id": file_id.into()}))
            .await
    }

    /// 上传群文件。`folder_id` 为空时传到根目录。
    pub async fn upload_group_file(
        &self,
        group_id: i64,
        file: impl Into<String>,
        name: impl Into<String>,
        folder_id: Option<String>,
    ) -> Result<()> {
        let mut params = json!({"group_id": group_id, "file": file.into(), "name": name.into()});
        if let Some(folder) = folder_id {
            params["folder"] = json!(folder);
        }
        self.fire("upload_group_file", params).await
    }

    /// 上传私聊文件。
    pub async fn upload_private_file(
        &self,
        user_id: i64,
        file: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<()> {
        self.fire(
            "upload_private_file",
            json!({"user_id": user_id, "file": file.into(), "name": name.into()}),
        )
        .await
    }

    /// 群文件根目录列表。
    pub async fn get_group_root_files(&self, group_id: i64) -> Result<Value> {
        self.query("get_group_root_files", json!({"group_id": group_id}))
            .await
    }

    /// 群文件子目录列表。
    pub async fn get_group_files_by_folder(
        &self,
        group_id: i64,
        folder_id: impl Into<String>,
    ) -> Result<Value> {
        self.query(
            "get_group_files_by_folder",
            json!({"group_id": group_id, "folder_id": folder_id.into()}),
        )
        .await
    }

    /// 群文件下载直链。`bus_id` 来自文件列表条目。
    pub async fn get_group_file_url(
        &self,
        group_id: i64,
        file_id: impl Into<String>,
        bus_id: i64,
    ) -> Result<Value> {
        self.query(
            "get_group_file_url",
            json!({"group_id": group_id, "file_id": file_id.into(), "busid": bus_id}),
        )
        .await
    }

    /// 创建群文件夹。
    pub async fn create_group_file_folder(
        &self,
        group_id: i64,
        name: impl Into<String>,
    ) -> Result<()> {
        self.fire(
            "create_group_file_folder",
            json!({"group_id": group_id, "name": name.into()}),
        )
        .await
    }

    /// 移动群文件到目标目录。
    pub async fn move_group_file(
        &self,
        group_id: i64,
        file_id: impl Into<String>,
        target_dir: impl Into<String>,
    ) -> Result<()> {
        self.fire(
            "move_group_file",
            json!({"group_id": group_id, "file_id": file_id.into(), "target_dir": target_dir.into()}),
        )
        .await
    }

    /// 删除群文件。
    pub async fn delete_group_file(
        &self,
        group_id: i64,
        file_id: impl Into<String>,
        bus_id: i64,
    ) -> Result<()> {
        self.fire(
            "delete_group_file",
            json!({"group_id": group_id, "file_id": file_id.into(), "busid": bus_id}),
        )
        .await
    }

    // ── 群管理（标准 v11 / go-cqhttp 扩展） ──

    /// 全员禁言开关。
    pub async fn set_group_whole_ban(&self, group_id: i64, enable: bool) -> Result<()> {
        self.fire(
            "set_group_whole_ban",
            json!({"group_id": group_id, "enable": enable}),
        )
        .await
    }

    /// 设置群名。
    pub async fn set_group_name(&self, group_id: i64, group_name: impl Into<String>) -> Result<()> {
        self.fire(
            "set_group_name",
            json!({"group_id": group_id, "group_name": group_name.into()}),
        )
        .await
    }

    /// 禁言匿名成员（标准 v11；NapCat 因 NTQQ 无匿名聊天而不支持，
    /// `anonymous_flag` 来自匿名消息事件）。
    pub async fn set_group_anonymous_ban(
        &self,
        group_id: i64,
        anonymous_flag: impl Into<String>,
        duration_secs: i32,
    ) -> Result<()> {
        self.fire(
            "set_group_anonymous_ban",
            json!({
                "group_id": group_id,
                "anonymous_flag": anonymous_flag.into(),
                "duration": duration_secs
            }),
        )
        .await
    }

    /// 开关群匿名聊天（标准 v11；NapCat 不支持，同上）。
    pub async fn set_group_anonymous(&self, group_id: i64, enable: bool) -> Result<()> {
        self.fire(
            "set_group_anonymous",
            json!({"group_id": group_id, "enable": enable}),
        )
        .await
    }

    /// 设置群成员专属头衔（群主权限）。
    pub async fn set_group_special_title(
        &self,
        group_id: i64,
        user_id: i64,
        special_title: impl Into<String>,
    ) -> Result<()> {
        self.fire(
            "set_group_special_title",
            json!({
                "group_id": group_id,
                "user_id": user_id,
                "special_title": special_title.into()
            }),
        )
        .await
    }

    /// 群荣誉信息。`honor_type`：talkative/performer/legend/strong_newer/emotion/all。
    pub async fn get_group_honor_info(&self, group_id: i64, honor_type: &str) -> Result<Value> {
        self.query(
            "get_group_honor_info",
            json!({"group_id": group_id, "type": honor_type}),
        )
        .await
    }

    /// 本群剩余 @全体成员 次数（群发前先查配额）。
    pub async fn get_group_at_all_remain(&self, group_id: i64) -> Result<Value> {
        self.query("get_group_at_all_remain", json!({"group_id": group_id}))
            .await
    }

    /// 群信息。
    pub async fn get_group_info(&self, group_id: i64, no_cache: bool) -> Result<Value> {
        self.query(
            "get_group_info",
            json!({"group_id": group_id, "no_cache": no_cache}),
        )
        .await
    }

    /// 群成员列表。
    pub async fn get_group_member_list(&self, group_id: i64) -> Result<Vec<Value>> {
        self.query_list("get_group_member_list", json!({"group_id": group_id}))
            .await
    }

    /// 陌生人信息。
    pub async fn get_stranger_info(&self, user_id: i64) -> Result<Value> {
        self.query("get_stranger_info", json!({"user_id": user_id}))
            .await
    }

    /// 登录号信息。
    pub async fn get_login_info(&self) -> Result<Value> {
        self.query("get_login_info", json!({})).await
    }

    /// 实现版本信息。
    pub async fn get_version_info(&self) -> Result<Value> {
        self.query("get_version_info", json!({})).await
    }

    /// 运行状态。
    pub async fn get_status(&self) -> Result<Value> {
        self.query("get_status", json!({})).await
    }

    /// 能否发图。
    pub async fn can_send_image(&self) -> Result<bool> {
        Ok(self
            .query("can_send_image", json!({}))
            .await?
            .get("yes")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    /// 能否发语音。
    pub async fn can_send_record(&self) -> Result<bool> {
        Ok(self
            .query("can_send_record", json!({}))
            .await?
            .get("yes")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    /// 重启实现（延迟秒数）。断开连接属预期行为。
    pub async fn set_restart(&self, delay_secs: i32) -> Result<()> {
        self.fire("set_restart", json!({"delay": delay_secs})).await
    }

    /// 清理缓存。
    pub async fn clean_cache(&self) -> Result<()> {
        self.fire("clean_cache", json!({})).await
    }

    /// 官方 QQ 机器人的 UIN 号段范围（用于识别消息是否来自官方机器人）。
    pub async fn get_robot_uin_range(&self) -> Result<Value> {
        self.query("get_robot_uin_range", json!({})).await
    }

    /// 设置机器人自身头像。
    pub async fn set_qq_avatar(&self, file: impl Into<String>) -> Result<()> {
        self.fire("set_qq_avatar", json!({"file": file.into()}))
            .await
    }
}

/// 原生消息封装：直接基于统一的领域消息段（[`crate::domain::MessageSegment`]），不提供绕过类型检查的平台原始 JSON 通道。
#[derive(Debug, Clone)]
pub struct NativeMessage(Vec<crate::domain::MessageSegment>);
impl NativeMessage {
    pub fn into_segments(self) -> Vec<crate::domain::MessageSegment> {
        self.0
    }
}
impl From<String> for NativeMessage {
    fn from(text: String) -> Self {
        Self(vec![crate::domain::MessageSegment::text(text)])
    }
}
impl From<&str> for NativeMessage {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}
impl From<crate::domain::MessageSegment> for NativeMessage {
    fn from(segment: crate::domain::MessageSegment) -> Self {
        Self(vec![segment])
    }
}
impl From<Vec<crate::domain::MessageSegment>> for NativeMessage {
    fn from(segments: Vec<crate::domain::MessageSegment>) -> Self {
        Self(segments)
    }
}

/// 可发送的消息类型
#[derive(Debug)]
#[non_exhaustive]
pub enum SendableMessage {
    /// 消息段
    Segment(crate::api::MessageSegment),
    /// 消息段数组
    Segments(Vec<crate::api::MessageSegment>),
    /// JSON 字符串
    Json(String),
    /// 文本
    Text(String),
}

impl SendableMessage {
    /// 该消息是否可能包含需要宿主统一处理的富消息段。
    ///
    /// `Json` 无法在不解码的前提下判断，只有它会触发完整解析；文本、单段和普通
    /// 段数组都能直接按变体判断，避免媒体消息为大字段付出额外的 JSON 编码成本。
    pub fn may_contain_rich_segments(&self) -> bool {
        match self {
            SendableMessage::Segment(seg) => seg.is_rich(),
            SendableMessage::Segments(segs) => segs.iter().any(crate::api::MessageSegment::is_rich),
            SendableMessage::Json(_) => true,
            SendableMessage::Text(_) => false,
        }
    }

    /// 消费自身转换为 OneBot 消息段 JSON。
    ///
    /// 线上形状与 [`Self::to_json`] 一致，但媒体字段按所有权移动；发送路径应优先
    /// 使用本方法，避免大段 Base64 在 `Value` 之间被反复深拷贝。
    pub fn into_json(self) -> Value {
        match self {
            SendableMessage::Segment(seg) => Value::Array(vec![seg.into_json()]),
            SendableMessage::Segments(segs) => Value::Array(
                segs.into_iter()
                    .map(crate::api::MessageSegment::into_json)
                    .collect(),
            ),
            SendableMessage::Json(s) => {
                if let Ok(v) = serde_json::from_str::<Value>(&s) {
                    match v {
                        Value::Object(ref object) if object.contains_key("type") => {
                            Value::Array(vec![v])
                        }
                        Value::String(text) => {
                            Value::Array(vec![json!({"type":"text","data":{"text":text}})])
                        }
                        other => other,
                    }
                } else {
                    Value::Array(vec![json!({"type": "text", "data": {"text": s}})])
                }
            }
            SendableMessage::Text(s) => {
                Value::Array(vec![json!({"type": "text", "data": {"text": s}})])
            }
        }
    }

    /// 转成 OneBot 消息段 JSON（数组形态）。
    pub fn to_json(&self) -> Value {
        match self {
            SendableMessage::Segment(seg) => json!([seg.to_json()]),
            SendableMessage::Segments(segs) => {
                json!(segs.iter().map(|s| s.to_json()).collect::<Vec<_>>())
            }
            SendableMessage::Json(s) => {
                if let Ok(v) = serde_json::from_str::<Value>(s) {
                    match v {
                        Value::Object(ref object) if object.contains_key("type") => json!([v]),
                        Value::String(text) => json!([{"type":"text","data":{"text":text}}]),
                        other => other,
                    }
                } else {
                    json!([{"type": "text", "data": {"text": s}}])
                }
            }
            SendableMessage::Text(s) => {
                json!([{"type": "text", "data": {"text": s}}])
            }
        }
    }
}

impl From<String> for SendableMessage {
    fn from(s: String) -> Self {
        SendableMessage::Text(s)
    }
}

impl From<&str> for SendableMessage {
    fn from(s: &str) -> Self {
        SendableMessage::Text(s.to_string())
    }
}

impl From<crate::api::MessageSegment> for SendableMessage {
    fn from(seg: crate::api::MessageSegment) -> Self {
        SendableMessage::Segment(seg)
    }
}

impl From<crate::api::MessageBuilder> for SendableMessage {
    fn from(builder: crate::api::MessageBuilder) -> Self {
        SendableMessage::Segments(builder.build())
    }
}

impl From<Vec<crate::api::MessageSegment>> for SendableMessage {
    fn from(segs: Vec<crate::api::MessageSegment>) -> Self {
        SendableMessage::Segments(segs)
    }
}

/// 消息目标
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MessageTarget {
    /// 私聊
    User(i64),
    /// 群聊
    Group(i64),
    /// 频道
    Channel(i64, i64), // (guild_id, channel_id)
}

/// 带完整上下文的目标发送参数
#[derive(Debug)]
pub struct TargetSendParams<'a> {
    pub msg: SendableMessage,
    pub target_type: &'a str,
    pub target_id: &'a str,
    pub bot_id: &'a str,
    pub bot_self_id: &'a str,
    pub msg_id: Option<&'a str>,
    pub at_sender: bool,
    pub sender_id: Option<&'a str>,
}

impl MessageTarget {
    /// 创建私聊目标
    pub fn user(user_id: i64) -> Self {
        MessageTarget::User(user_id)
    }

    /// 创建群聊目标
    pub fn group(group_id: i64) -> Self {
        MessageTarget::Group(group_id)
    }

    /// 创建频道目标
    pub fn channel(guild_id: i64, channel_id: i64) -> Self {
        MessageTarget::Channel(guild_id, channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{Bot, MessageContext};
    use crate::api::waiter::WaitManager;
    use crate::core::event_bus::EventBus;
    use crate::core::event_kernel::RuntimeActionSink;
    use crate::core::onebot_client::ApiClient;
    use crate::core::{HostLogLevel, RuntimeActionRouter, RuntimeBotRoute};
    use crate::error::Result;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};

    #[test]
    fn into_json_matches_to_json_for_every_variant() {
        use crate::api::MessageSegment;
        use crate::api::bot::SendableMessage;

        let cases = vec![
            SendableMessage::Text("纯文本".into()),
            SendableMessage::Segment(MessageSegment::image("base64://AAAA")),
            SendableMessage::Segment(MessageSegment::image_with_url(
                "base64://AAAA",
                "https://example.test/a.png",
            )),
            SendableMessage::Segment(MessageSegment::video("url://example")),
            SendableMessage::Segment(MessageSegment::Record {
                file: "file:///tmp/a.mp3".into(),
                url: Some("https://example.test/a.mp3".into()),
                magic: Some(true),
            }),
            SendableMessage::Segment(MessageSegment::file("/tmp/a.bin", "a.bin")),
            SendableMessage::Segment(MessageSegment::at_all()),
            SendableMessage::Segments(vec![
                MessageSegment::text("标题"),
                MessageSegment::record("file:///tmp/a.mp3"),
            ]),
            SendableMessage::Json(
                serde_json::json!([{"type":"video","data":{"file":"base64://AAAA"}}]).to_string(),
            ),
            SendableMessage::Json("[not json]".into()),
        ];
        for message in cases {
            let expected = message.to_json();
            let owned = message.into_json();
            assert_eq!(expected, owned);
        }
    }

    #[derive(Default)]
    struct HostLogVisitor {
        plugin_target: String,
        message: String,
        fields: String,
    }

    impl Visit for HostLogVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            match field.name() {
                "plugin_target" => {
                    self.plugin_target = format!("{value:?}").trim_matches('"').into()
                }
                "message" => self.message = format!("{value:?}").trim_matches('"').into(),
                "fields_json" => self.fields = format!("{value:?}"),
                _ => {}
            }
        }
    }

    struct HostLogCapture(Arc<Mutex<Vec<(tracing::Level, HostLogVisitor)>>>);

    impl<S> Layer<S> for HostLogCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            if event.metadata().target() != "nuono_plugin::host_log" {
                return;
            }
            let mut visitor = HostLogVisitor::default();
            event.record(&mut visitor);
            self.0
                .lock()
                .unwrap()
                .push((*event.metadata().level(), visitor));
        }
    }

    struct QueryActionSink;

    /// 记录每次 fire 出去的 action 名与参数，用于钉死参数形状。
    struct CaptureSink(Arc<Mutex<Vec<Value>>>);

    #[async_trait::async_trait]
    impl RuntimeActionSink for CaptureSink {
        async fn send_action(&self, action: Value) -> Result<()> {
            self.0.lock().unwrap().push(action);
            Ok(())
        }

        async fn call_action(&self, action: Value) -> Result<Value> {
            self.0.lock().unwrap().push(action);
            Ok(Value::Null)
        }
    }

    fn captured(sent: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
        std::mem::take(&mut *sent.lock().unwrap())
    }

    #[async_trait::async_trait]
    impl RuntimeActionSink for QueryActionSink {
        async fn send_action(&self, _action: Value) -> Result<()> {
            Ok(())
        }

        async fn call_action(&self, action: Value) -> Result<Value> {
            Ok(match action["action"].as_str().unwrap_or_default() {
                "get_group_member_info" => serde_json::json!({"user_id": 2}),
                "get_friend_list" => serde_json::json!([{"user_id": 2}]),
                "get_group_list" => serde_json::json!([{"group_id": 1}]),
                _ => Value::Null,
            })
        }
    }

    fn test_bot(action_sink: Option<Arc<dyn RuntimeActionSink>>) -> Bot {
        Bot::new(
            123456,
            Arc::new(EventBus::new()),
            Arc::new(WaitManager::new()),
            Arc::new(ApiClient::new()),
            None,
            action_sink,
        )
    }

    fn qq_context(message_type: &str) -> MessageContext {
        MessageContext::from_onebot_json(&serde_json::json!({
            "post_type": "message",
            "self_id": 123456,
            "user_id": "member-openid",
            "group_id": (message_type == "group").then_some("group-openid"),
            "message_id": "msg-1",
            "message_type": message_type,
            "raw_message": "hello",
            "openid": "member-openid",
            "group_openid": (message_type == "group").then_some("group-openid"),
            "event_id": "event-1",
            "qq_target_type": if message_type == "group" { "group" } else { "c2c" }
        }))
        .expect("message context")
    }

    #[tokio::test]
    async fn prepared_reply_event_keeps_immediate_reply_locale_and_native_ids() {
        use crate::api::waiter::WaitEvent;
        use crate::{Locale, LocaleContext, LocaleSource};
        use std::time::Duration;

        let mut bot = test_bot(None).with_wait_owner("DemoPlugin", None);
        bot.set_current_message(qq_context("group"));
        let registration = bot
            .prepare_reply_event_wait(
                Duration::from_secs(1),
                crate::api::waiter::ReplyWaitMode::Multiple,
            )
            .unwrap();
        let snapshot = registration.snapshot().clone();
        assert_eq!(snapshot.session_id, "123456:group-openid:member-openid");
        assert_eq!(snapshot.scope.plugin_name.as_deref(), Some("DemoPlugin"));
        assert_eq!(snapshot.scope.user_id, "member-openid");
        assert_eq!(snapshot.scope.group_id.as_deref(), Some("group-openid"));
        assert!(snapshot.scope.multiply);
        let locale = LocaleContext {
            locale: Locale::Ko,
            source: LocaleSource::User,
        };
        let raw = json!({
            "user_id":"member-openid", "group_id":"group-openid",
            "message_id":"opaque-reply", "raw_message":"[图片]",
            "message":[{"type":"image","data":{"file":"file_id:card"}}]
        });
        // 在接收端 Future 被首次 poll 之前，用户回复可能在提示发出后立刻到达。
        assert!(bot.wait_manager.respond_with_context(
            &snapshot.session_id,
            WaitEvent {
                raw: raw.clone(),
                domain: None,
                locale
            }
        ));
        let reply = registration.receive(Duration::from_secs(1)).await.unwrap();
        assert_eq!(reply.event().locale_context(), locale);
        assert_eq!(reply.event().raw(), &raw);
        assert_eq!(
            reply.event().user_id_str().as_deref(),
            Some("member-openid")
        );
        assert_eq!(
            reply.event().message_id_str().as_deref(),
            Some("opaque-reply")
        );
        assert_eq!(bot.wait_manager.wait_count(), 0);
    }

    #[tokio::test]
    async fn dropping_prepared_reply_event_cleans_only_its_generation() {
        use std::time::Duration;

        let mut bot = test_bot(None).with_wait_owner("DemoPlugin", None);
        bot.set_current_message(qq_context("group"));
        let previous = bot
            .prepare_reply_event_wait(
                Duration::from_secs(1),
                crate::api::waiter::ReplyWaitMode::Single,
            )
            .unwrap();
        let session = previous.snapshot().session_id.clone();
        let current = bot
            .prepare_reply_event_wait(
                Duration::from_secs(1),
                crate::api::waiter::ReplyWaitMode::Single,
            )
            .unwrap();
        let generation = current.snapshot().generation;
        drop(previous);
        assert_eq!(
            bot.wait_manager.snapshot(&session).unwrap().generation,
            generation
        );
        drop(current);
        assert!(!bot.wait_manager.has_wait(&session));
        assert_eq!(bot.wait_manager.wait_count(), 0);
    }

    #[test]
    fn message_context_keeps_qq_reply_fields() {
        let context = qq_context("group");
        assert_eq!(context.self_id, 123456);
        assert_eq!(context.user_id, 0);
        assert_eq!(context.group_id, None);
        assert_eq!(context.reply_context["openid"], "member-openid");
        assert_eq!(context.reply_context["group_openid"], "group-openid");
        assert_eq!(context.reply_context["event_id"], "event-1");
    }

    #[test]
    fn build_reply_api_call_uses_qq_native_targets() {
        let mut bot = test_bot(None);
        bot.set_current_message(qq_context("group"));
        let group: Value = serde_json::from_str(
            &bot.build_reply_api_call("pong".into())
                .expect("group reply"),
        )
        .expect("group action json");
        assert_eq!(group["params"]["group_id"], "group-openid");
        assert_eq!(group["params"]["openid"], "member-openid");
        assert_eq!(group["params"]["message_id"], "msg-1");

        bot.set_current_message(qq_context("private"));
        let private: Value = serde_json::from_str(
            &bot.build_reply_api_call("pong".into())
                .expect("private reply"),
        )
        .expect("private action json");
        assert_eq!(private["params"]["user_id"], "member-openid");
        assert_eq!(private["params"]["qq_target_type"], "c2c");
    }

    #[tokio::test]
    async fn query_actions_use_runtime_sink_without_websocket() {
        let sink: Arc<dyn RuntimeActionSink> = Arc::new(QueryActionSink);
        let bot = test_bot(Some(sink));

        assert_eq!(bot.get_group_member_info(1, 2).await.unwrap()["user_id"], 2);
        assert_eq!(bot.get_friend_list().await.unwrap()[0]["user_id"], 2);
        assert_eq!(bot.get_group_list().await.unwrap()[0]["group_id"], 1);
    }

    /// 参数名核对自 NapCat 文档；错一个字段实现就会静默失败。
    #[tokio::test]
    async fn new_action_helpers_send_documented_param_shapes() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let bot = test_bot(Some(Arc::new(CaptureSink(sent.clone()))));

        bot.set_msg_emoji_like(-2147000001, "76").await.unwrap();
        bot.mark_group_msg_as_read(100).await.unwrap();
        bot.mark_private_msg_as_read(200).await.unwrap();
        bot.mark_all_as_read().await.unwrap();
        bot.friend_poke(300).await.unwrap();
        bot.group_poke(100, 300).await.unwrap();
        bot.set_group_whole_ban(100, true).await.unwrap();
        bot.set_essence_msg(-2147000002).await.unwrap();
        bot.get_file("file-abc").await.unwrap();
        bot.forward_group_single_msg(100, -2147000003)
            .await
            .unwrap();
        bot.set_qq_avatar("file:///tmp/a.jpg").await.unwrap();

        let sent = captured(&sent);
        let by_action = |name: &str| {
            sent.iter()
                .find(|a| a["action"] == name)
                .unwrap_or_else(|| panic!("action {name} not sent"))
                .clone()
        };
        assert_eq!(
            by_action("set_msg_emoji_like")["params"],
            json!({"message_id": -2147000001, "emoji_id": "76"})
        );
        assert_eq!(
            by_action("mark_group_msg_as_read")["params"],
            json!({"group_id": 100})
        );
        assert_eq!(by_action("_mark_all_as_read")["params"], json!({}));
        assert_eq!(
            by_action("group_poke")["params"],
            json!({"group_id": 100, "user_id": 300})
        );
        assert_eq!(
            by_action("set_group_whole_ban")["params"],
            json!({"group_id": 100, "enable": true})
        );
        assert_eq!(
            by_action("get_file")["params"],
            json!({"file_id": "file-abc"})
        );
        assert_eq!(
            by_action("set_qq_avatar")["params"],
            json!({"file": "file:///tmp/a.jpg"})
        );
    }

    #[tokio::test]
    async fn group_file_and_history_helpers_match_napcat_fields() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let bot = test_bot(Some(Arc::new(CaptureSink(sent.clone()))));

        bot.get_group_file_url(100, "fid-1", 3).await.unwrap();
        bot.delete_group_file(100, "fid-1", 3).await.unwrap();
        bot.move_group_file(100, "fid-1", "/资料").await.unwrap();
        bot.create_group_file_folder(100, "资料").await.unwrap();
        bot.get_group_at_all_remain(100).await.unwrap();
        bot.get_friend_msg_history(9, None, 20).await.unwrap();
        bot.get_group_msg_history(100, Some(55), 10).await.unwrap();
        bot.upload_group_file(100, "file:///tmp/v.mp4", "v.mp4", Some("/资料".to_string()))
            .await
            .unwrap();
        bot.upload_private_file(9, "file:///tmp/v.mp4", "v.mp4")
            .await
            .unwrap();
        bot.set_group_anonymous_ban(100, "flag-x", 600)
            .await
            .unwrap();

        let sent = captured(&sent);
        let params_of = |name: &str| {
            sent.iter()
                .find(|a| a["action"] == name)
                .unwrap_or_else(|| panic!("action {name} not sent"))["params"]
                .clone()
        };
        // NapCat 的 busid 没有下划线，target_dir 而非 parent_id——照文档原样。
        assert_eq!(
            params_of("get_group_file_url"),
            json!({"group_id": 100, "file_id": "fid-1", "busid": 3})
        );
        assert_eq!(
            params_of("move_group_file"),
            json!({"group_id": 100, "file_id": "fid-1", "target_dir": "/资料"})
        );
        assert_eq!(
            params_of("upload_group_file"),
            json!({"group_id": 100, "file": "file:///tmp/v.mp4", "name": "v.mp4", "folder": "/资料"})
        );
        assert_eq!(
            params_of("get_group_msg_history"),
            json!({"group_id": 100, "message_seq": 55, "count": 10})
        );
        assert_eq!(
            params_of("set_group_anonymous_ban"),
            json!({"group_id": 100, "anonymous_flag": "flag-x", "duration": 600})
        );
    }

    #[tokio::test]
    async fn forward_send_encodes_node_segments() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let bot = test_bot(Some(Arc::new(CaptureSink(sent.clone()))));

        let nodes = vec![
            crate::api::MessageSegment::node_with_info(
                "张三",
                10001,
                vec![crate::api::MessageSegment::text("第一条")],
            ),
            crate::api::MessageSegment::node(vec![
                crate::api::MessageSegment::at_all(),
                crate::api::MessageSegment::text("第二条"),
            ]),
        ];
        bot.send_group_forward_msg(100, &nodes).await.unwrap();

        let sent = captured(&sent);
        let params = &sent[0]["params"];
        assert_eq!(params["group_id"], 100);
        assert_eq!(params["messages"][0]["type"], "node");
        assert_eq!(params["messages"][0]["data"]["name"], "张三");
        assert_eq!(
            params["messages"][0]["data"]["content"][0]["data"]["text"],
            "第一条"
        );
        // 无署名节点省略 name/uin；嵌套 at_all 仍是 qq:"all"
        assert!(params["messages"][1]["data"].get("name").is_none());
        assert_eq!(
            params["messages"][1]["data"]["content"][0]["data"]["qq"],
            "all"
        );
    }

    #[test]
    fn host_log_api_dispatches_all_levels_to_captured_host_subscriber() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(HostLogCapture(captured.clone()));
        let router =
            tracing::subscriber::with_default(subscriber, || Arc::new(RuntimeActionRouter::new()));
        let bot = test_bot(None).with_action_router(router);

        for level in [
            HostLogLevel::Trace,
            HostLogLevel::Debug,
            HostLogLevel::Info,
            HostLogLevel::Warn,
            HostLogLevel::Error,
        ] {
            bot.host_log(
                level,
                "plugin.test",
                format!("{level:?}"),
                serde_json::json!({"request_id": "req-1"}),
            )
            .unwrap();
        }

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 5);
        assert_eq!(captured[0].0, tracing::Level::TRACE);
        assert_eq!(captured[1].0, tracing::Level::DEBUG);
        assert_eq!(captured[2].0, tracing::Level::INFO);
        assert_eq!(captured[3].0, tracing::Level::WARN);
        assert_eq!(captured[4].0, tracing::Level::ERROR);
        assert!(
            captured
                .iter()
                .all(|(_, event)| event.plugin_target == "plugin.test")
        );
        assert!(
            captured
                .iter()
                .all(|(_, event)| event.fields.contains("request_id"))
        );
        assert_eq!(captured[2].1.message, "Info");
    }

    #[test]
    fn host_log_requires_router_capability() {
        let error = test_bot(None)
            .host_info("plugin.test", "message", Value::Null)
            .unwrap_err();
        assert!(matches!(
            error,
            crate::error::NuonoError::MessageChannelNotSet
        ));
    }

    #[tokio::test]
    async fn routed_api_uses_exact_native_route_and_receipt() {
        let router = Arc::new(RuntimeActionRouter::new());
        let route = RuntimeBotRoute::new("qq_official", "instance-x", "app-id-x");
        router.register(route.clone(), Arc::new(QueryActionSink));
        let bot = test_bot(None).with_action_router(router);
        assert_eq!(bot.connected_routes().unwrap(), vec![route.clone()]);

        let value = bot
            .call_routed(
                &route,
                serde_json::json!({"action": "get_group_list", "params": {}}),
            )
            .await
            .unwrap();
        assert_eq!(value[0]["group_id"], 1);
        bot.notify_routed(
            &route,
            serde_json::json!({"action": "send_msg", "params": {}}),
        )
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_plugin_sends_keep_origin_across_host_runtime_dispatch() {
        struct OriginSink;
        #[async_trait::async_trait]
        impl RuntimeActionSink for OriginSink {
            async fn send_action(&self, _: Value) -> Result<()> {
                Ok(())
            }
            async fn call_action(&self, _: Value) -> Result<Value> {
                tokio::task::yield_now().await;
                Ok(json!({"plugin": crate::core::action_log::current_plugin()}))
            }
        }
        let router = Arc::new(RuntimeActionRouter::new());
        let route = RuntimeBotRoute::new("qq_official", "origin-test", "bot");
        router.register(route.clone(), Arc::new(OriginSink));
        let first = test_bot(None)
            .with_action_router(router.clone())
            .with_wait_owner("DemoPlugin", None);
        let second = test_bot(None)
            .with_action_router(router)
            .with_wait_owner("BetaPlugin", None);
        let action = json!({"action":"get_status","params":{}});
        let (a, b) = tokio::join!(
            first.call_routed(&route, action.clone()),
            second.call_routed(&route, action)
        );
        assert_eq!(a.unwrap()["plugin"], "DemoPlugin");
        assert_eq!(b.unwrap()["plugin"], "BetaPlugin");
        assert!(crate::core::action_log::current_plugin().is_none());
    }

    /// 主动发送：record 型 sink 记录 action 并返回精确 receipt。
    struct RecordRoutedSink(Arc<Mutex<Vec<Value>>>, Value);

    #[async_trait::async_trait]
    impl RuntimeActionSink for RecordRoutedSink {
        async fn send_action(&self, action: Value) -> Result<()> {
            self.0.lock().unwrap().push(action);
            Ok(())
        }

        async fn call_action(&self, action: Value) -> Result<Value> {
            self.0.lock().unwrap().push(action);
            Ok(self.1.clone())
        }
    }

    fn routed_bot(
        route: crate::core::RuntimeBotRoute,
        reply: Value,
    ) -> (Bot, Arc<Mutex<Vec<Value>>>) {
        let router = Arc::new(RuntimeActionRouter::new());
        let sent = Arc::new(Mutex::new(Vec::new()));
        router.register(route, Arc::new(RecordRoutedSink(sent.clone(), reply)));
        let bot = test_bot(None).with_action_router(router);
        (bot, sent)
    }

    #[tokio::test]
    async fn native_send_preserves_scope_and_qq_alias_and_rejects_group_actor_before_dispatch() {
        use crate::domain::{DeliveryTarget, IdentityKind, IdentityRef, IdentityScheme, Platform};
        let route = RuntimeBotRoute::new("qq", "instance-x", "app-id-x");
        let (bot, sent) = routed_bot(
            route.clone(),
            json!({"message_id":"000001","raw":{"ok":true}}),
        );
        let actor = |namespace: &str| {
            DeliveryTarget::actor(
                IdentityRef::new(
                    Platform::QqOfficial,
                    IdentityKind::User,
                    IdentityScheme::OpenId,
                    "same-opaque",
                    Some(namespace),
                )
                .unwrap(),
            )
            .unwrap()
        };
        let receipt = bot
            .send_native_routed(&route, &actor("app-id-x"), "hi")
            .await
            .unwrap();
        assert_eq!(receipt.message_ids, ["000001"]);
        assert_eq!(receipt.raw, json!({"ok":true}));
        bot.send_native_routed_notify(&route, &actor("app-id-x"), "notify")
            .await
            .unwrap();
        for namespace in [
            "app-id-y",
            "app-id-x/groups/group-a",
            "app-id-x/groups/group-b",
        ] {
            assert!(
                bot.send_native_routed(&route, &actor(namespace), "wrong")
                    .await
                    .is_err()
            );
            assert!(
                bot.send_native_routed_notify(&route, &actor(namespace), "wrong")
                    .await
                    .is_err()
            );
        }
        assert_eq!(sent.lock().unwrap().len(), 2);
        assert_eq!(sent.lock().unwrap()[0]["params"]["openid"], "same-opaque");
    }

    #[tokio::test]
    async fn send_routed_builds_canonical_action_and_returns_receipt() {
        use crate::api::routed::RuntimeMessageTarget;
        let route = crate::core::RuntimeBotRoute::new("qq_official", "instance-x", "app-id-x");
        let (bot, sent) = routed_bot(
            route.clone(),
            serde_json::json!({
                "message_ids": ["msg-1", "msg-2"],
                "raw": {"native": "payload"}
            }),
        );

        let receipt = bot
            .send_routed(&RuntimeMessageTarget::private(route, "member-openid"), "hi")
            .await
            .unwrap();

        // receipt 保留精确字符串 message IDs 与 raw。
        assert_eq!(
            receipt.message_ids,
            vec!["msg-1".to_string(), "msg-2".to_string()]
        );
        assert_eq!(receipt.raw, serde_json::json!({"native": "payload"}));

        // 记录的是框架规范的 send_msg action，含平台可解析的目标键。
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["action"], "send_msg");
        assert_eq!(sent[0]["params"]["openid"], "member-openid");
        assert_eq!(
            sent[0]["params"]["message"],
            serde_json::json!([{"type": "text", "data": {"text": "hi"}}])
        );
    }

    #[tokio::test]
    async fn send_routed_notify_does_not_wait_for_receipt() {
        use crate::api::routed::RuntimeMessageTarget;
        let route = crate::core::RuntimeBotRoute::new("qq_official", "instance-x", "app-id-x");
        let (bot, sent) = routed_bot(route.clone(), Value::Null);

        bot.send_routed_notify(&RuntimeMessageTarget::private(route, "member-openid"), "hi")
            .await
            .unwrap();

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["action"], "send_msg");
        assert_eq!(sent[0]["params"]["openid"], "member-openid");
    }

    #[tokio::test]
    async fn send_routed_returns_exact_string_ids_from_receipt() {
        use crate::api::routed::RuntimeMessageTarget;
        let route = crate::core::RuntimeBotRoute::new("discord", "instance-x", "app-id-x");
        let (bot, _sent) = routed_bot(
            route.clone(),
            serde_json::json!({"message_id": "18446744073709551615"}),
        );

        let receipt = bot
            .send_routed(
                &RuntimeMessageTarget::channel(route, "channel", Some("guild")),
                "hi",
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.message_ids,
            vec!["18446744073709551615".to_string()]
        );
    }

    #[tokio::test]
    async fn send_routed_offline_route_returns_clear_error() {
        use crate::api::routed::RuntimeMessageTarget;
        let route = crate::core::RuntimeBotRoute::new("telegram", "instance-x", "app-id-x");
        let (bot, _sent) = routed_bot(
            crate::core::RuntimeBotRoute::new("qq_official", "i", "b"),
            Value::Null,
        );

        let err = bot
            .send_routed(&RuntimeMessageTarget::private(route, "1"), "hi")
            .await
            .unwrap_err();
        assert!(matches!(err, crate::error::NuonoError::Connection(_)));
    }
}
