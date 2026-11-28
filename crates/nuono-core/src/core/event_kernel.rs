use super::message_log::{
    LoggingActionTransport, LoggingRuntimeActionSink, event_bot_id, inbound_summary, log_receive,
};
use super::runtime_outbound::send_transport_runtime_action;
use super::runtime_stages::{
    AgentRuntimeStage, CoreCommandsRuntimeStage, EventPublishRuntimeStage, FilterRuntimeStage,
    MatcherRuntimeStage, PluginHostRuntimeStage, TriggerDispatchRuntimeStage,
    WaitSessionRuntimeStage,
};
use crate::api::waiter::WaitManager;
use crate::core::transport::ActionTransport;
use crate::core::{
    event_bus::EventBus,
    filter::FilterHandler,
    handler::EventHandler,
    matcher::MatcherRegistry,
    permission::PermissionManager,
    platform::{PlatformAdapter, UnifiedEvent},
    plugin_host::{PluginHost, PluginMessageRoute},
    task_executor::TaskExecutor,
};
use crate::db::Database;
use crate::domain::DomainEvent;
use crate::error::Result;
use nuono_app::transport::TransportFrame;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;

#[cfg(test)]
#[path = "../../tests/unit/core/localization_tests.rs"]
mod localization_tests;

#[path = "interaction_runtime.rs"]
mod interaction_runtime;

impl nuono_app::pipeline::StageContext for RuntimeEventContext<'_> {
    fn processing_state(&self) -> nuono_app::pipeline::ProcessingState {
        nuono_app::pipeline::ProcessingState {
            is_message: self.is_message,
            consumed: self.blocked,
        }
    }
}

#[async_trait::async_trait]
impl<'a> nuono_app::pipeline::StageExecutor<RuntimeEventContext<'a>> for EventKernel {
    async fn execute_stage(
        &self,
        stage: RuntimeEventStage,
        ctx: &mut RuntimeEventContext<'a>,
    ) -> std::result::Result<(), nuono_app::pipeline::StageFailure> {
        if super::interaction::is_command() && !super::interaction::is_current() {
            ctx.blocked = true;
            return Ok(());
        }
        // 带有已校验签名的指令按钮点击事件直接归属到对应插件，即使该插件配置了空指令前缀也不会被当作普通文本误触发。
        if super::interaction::is_command()
            && matches!(
                stage,
                RuntimeEventStage::WaitSession
                    | RuntimeEventStage::CoreCommands
                    | RuntimeEventStage::Matcher
                    | RuntimeEventStage::AgentRuntime
            )
        {
            return Ok(());
        }
        // 显式插件命令拥有自己的命名空间，等待中的其他会话及全局兜底不能抢占。
        // 这是路由选择，不是消费消息；继续保留目标插件自己的 block 和最终发布语义。
        if ctx.is_message
            && matches!(
                stage,
                RuntimeEventStage::WaitSession
                    | RuntimeEventStage::Matcher
                    | RuntimeEventStage::AgentRuntime
            )
        {
            let route = self
                .plugin_host
                .message_route(
                    ctx.event
                        .get("raw_message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(""),
                )
                .await;
            if route.is_explicit()
                && !(stage == RuntimeEventStage::AgentRuntime
                    && route == PluginMessageRoute::FrameworkAgent)
            {
                return Ok(());
            }
        }
        let result = match stage {
            RuntimeEventStage::WaitSession => self.wait_session_runtime_stage.handle(ctx).await,
            RuntimeEventStage::Filter => self.filter_runtime_stage.handle(ctx).await,
            RuntimeEventStage::CoreCommands => {
                if ctx.web_chat_account_id.is_none()
                    && self.assistant.handle_binding_event(ctx).await
                {
                    ctx.blocked = true;
                    return Ok(());
                }
                let should_resolve = Self::is_private_message(&ctx.event)
                    || Self::looks_like_core_or_explicit_command(
                        ctx.event
                            .get("raw_message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or(""),
                    );
                if should_resolve && !self.ensure_interactive_user_resolved(ctx).await {
                    return Ok(());
                }
                let result = self.core_commands_runtime_stage.handle(ctx).await;
                if result.is_ok() && !ctx.blocked {
                    let route = self
                        .plugin_host
                        .message_route(
                            ctx.event
                                .get("raw_message")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or(""),
                        )
                        .await;
                    if let Some(message) = route.conflict_message() {
                        warn!(bot_id = %ctx.bot_id, route = ?route, "插件消息前缀冲突，拒绝分发");
                        ctx.send_reply_text(message, "PluginRouting").await;
                        ctx.blocked = true;
                    }
                }
                result
            }
            RuntimeEventStage::PluginHost => {
                let should_resolve = Self::is_private_message(&ctx.event)
                    || super::interaction::is_command()
                    || self
                        .plugin_host
                        .message_route(
                            ctx.event
                                .get("raw_message")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or(""),
                        )
                        .await
                        .is_explicit();
                if should_resolve && !self.ensure_interactive_user_resolved(ctx).await {
                    return Ok(());
                }
                self.plugin_host_runtime_stage.handle(ctx).await
            }
            RuntimeEventStage::TriggerDispatch => {
                let should_resolve = Self::is_private_message(&ctx.event)
                    || super::interaction::is_command()
                    || self
                        .plugin_host
                        .message_route(
                            ctx.event
                                .get("raw_message")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or(""),
                        )
                        .await
                        .is_explicit();
                if should_resolve && !self.ensure_interactive_user_resolved(ctx).await {
                    return Ok(());
                }
                self.trigger_dispatch_runtime_stage.handle(ctx).await
            }
            RuntimeEventStage::Matcher => self.matcher_runtime_stage.handle(ctx).await,
            RuntimeEventStage::AgentRuntime => {
                if let Some(account_id) = ctx.web_chat_account_id {
                    self.assistant.handle_web_chat_event(ctx, account_id).await;
                    ctx.blocked = true;
                    Ok(())
                } else {
                    let will_trigger_ai =
                        Self::is_private_message(&ctx.event) || Self::is_group_ai_trigger(ctx);
                    if will_trigger_ai && !self.ensure_interactive_user_resolved(ctx).await {
                        return Ok(());
                    }
                    if self.assistant.handle_event(ctx).await {
                        ctx.blocked = true;
                        Ok(())
                    } else {
                        self.agent_runtime_stage.handle(ctx).await
                    }
                }
            }
            RuntimeEventStage::EventPublish => self.event_publish_runtime_stage.handle(ctx).await,
        };
        result.map_err(|error| {
            warn!(bot_id = %ctx.bot_id, error = %error, stage = %stage, "runtime stage 执行失败");
            nuono_app::pipeline::StageFailure::new(error.kind())
        })
    }
}

pub use nuono_app::pipeline::{RuntimeEventPipeline, RuntimeEventProcessResult, RuntimeEventStage};

/// 当前入站运行时事件上下文。
///
/// `event` 始终是供既有插件/触发器消费的 OneBot 形状 JSON：OneBot 入站时它基本是
/// 原包，其他平台则是有损兼容投影；原平台负载保存在 `UnifiedEvent::raw`，并同时写入
/// 兼容 JSON 的 `raw` 字段。`bot_sender` 与 `action_sink` 互斥地表示两种出站方式：前者
/// 把 OneBot action 写回同一 WebSocket，后者交给平台专用 sink 翻译并调用 HTTP API。
pub struct RuntimeEventContext<'a> {
    /// 在事件入口处解析一次，后续所有运行时阶段共用同一份语言上下文。
    pub locale: nuono_app::localization::LocaleContext,
    pub bot_id: i64,
    pub raw_text: &'a str,
    pub event: serde_json::Value,
    pub bot_sender: Option<mpsc::Sender<TransportFrame>>,
    pub action_sink: Option<Arc<dyn RuntimeActionSink>>,
    pub action_transport: Option<Arc<dyn ActionTransport>>,
    pub domain_event: Option<DomainEvent>,
    pub unified_event: Option<UnifiedEvent>,
    /// 交互懒创建解析出的内部用户 ID（群聊被动闲聊未触发交互时保持为 `None`）。
    pub resolved_user_id: Option<i64>,
    /// 交互懒创建解析出的平台身份主键 ID（`nuono_platform_identities.id`）。
    pub resolved_identity_id: Option<i64>,
    /// 经服务端核验的网页对话所属站点账号 ID（仅限已登录账号的网页私聊会话，测试沙盒与外部平台保持 `None`）。
    pub web_chat_account_id: Option<i64>,
    /// 消费语义，不是错误语义。置位后后续消息阶段不再运行；阶段返回 `Err` 本身不会
    /// 自动置位，runtime 调度器会记录错误并按 fail-open 方式继续。
    pub blocked: bool,
    pub is_message: bool,
    /// 本消息累计的回复发送尝试与成功次数，不参与消费语义。
    pub reply_attempts: std::sync::atomic::AtomicUsize,
    pub reply_ok: std::sync::atomic::AtomicUsize,
}

impl RuntimeEventContext<'_> {
    pub(crate) fn reply_context(&self) -> RuntimeEventContext<'static> {
        RuntimeEventContext {
            locale: self.locale,
            bot_id: self.bot_id,
            raw_text: "",
            event: self.event.clone(),
            domain_event: self.domain_event.clone(),
            bot_sender: self.bot_sender.clone(),
            action_sink: self.action_sink.clone(),
            action_transport: self.action_transport.clone(),
            unified_event: None,
            resolved_user_id: self.resolved_user_id,
            resolved_identity_id: self.resolved_identity_id,
            web_chat_account_id: self.web_chat_account_id,
            blocked: false,
            is_message: true,
            reply_attempts: std::sync::atomic::AtomicUsize::new(0),
            reply_ok: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) async fn send_text(&self, text: String, stage: &str) -> bool {
        use std::sync::atomic::Ordering;
        self.reply_attempts.fetch_add(1, Ordering::Relaxed);
        let ok = self.send_text_inner(text, stage).await;
        if ok {
            self.reply_ok.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    pub(crate) async fn send_reply_text(&self, text: String, stage: &str) -> bool {
        self.send_text(self.build_text_reply_action(text).to_string(), stage)
            .await
    }

    pub(crate) async fn send_reply_image(&self, bytes: &[u8], stage: &str) -> bool {
        use base64::Engine;
        let mut action = self.build_text_reply_action(String::new());
        action["params"]["message"] = serde_json::json!([{
            "type": "image",
            "data": {"file": format!("base64://{}", base64::engine::general_purpose::STANDARD.encode(bytes))}
        }]);
        self.send_text(action.to_string(), stage).await
    }
    async fn send_text_inner(&self, text: String, stage: &str) -> bool {
        if let (Some(action_transport), Some(domain_event)) =
            (&self.action_transport, &self.domain_event)
        {
            let action = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .filter(|value| value.get("action").is_some())
                .unwrap_or_else(|| self.build_text_reply_action(text));
            return send_transport_runtime_action(
                action_transport.as_ref(),
                &domain_event.bot,
                Some(domain_event),
                action,
                stage,
            )
            .await;
        }

        if let Some(sender) = &self.bot_sender {
            let action = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .filter(|value| value.get("action").is_some())
                .map(|mut value| {
                    self.apply_reply_context(&mut value);
                    value.to_string()
                })
                .unwrap_or(text);
            return sender.send(TransportFrame::Text(action)).await.is_ok();
        }

        let Some(action_sink) = &self.action_sink else {
            warn!(
                bot_id = %self.bot_id,
                stage,
                "运行时事件没有平台发送通道，跳过回复发送"
            );
            return false;
        };

        let action = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .filter(|value| value.get("action").is_some())
            .map(|mut value| {
                self.apply_reply_context(&mut value);
                value
            })
            .unwrap_or_else(|| self.build_text_reply_action(text));

        match action_sink.send_action(action).await {
            Ok(()) => true,
            Err(e) => {
                warn!(
                    bot_id = %self.bot_id,
                    stage,
                    error = %e,
                    "运行时事件发送失败"
                );
                false
            }
        }
    }

    fn build_text_reply_action(&self, text: String) -> serde_json::Value {
        let message_type = self
            .event
            .get("message_type")
            .and_then(|m| m.as_str())
            .unwrap_or("private");
        let mut params = if message_type == "group" || message_type == "channel" {
            serde_json::json!({
                "message_type": message_type,
                "group_id": self.event.get("group_id").cloned().unwrap_or_else(|| serde_json::json!(0)),
                "message": text,
            })
        } else {
            serde_json::json!({
                "message_type": "private",
                "user_id": self.event.get("user_id").cloned().unwrap_or_else(|| serde_json::json!(0)),
                "message": text,
            })
        };
        if let Some(params_object) = params.as_object_mut() {
            Self::copy_reply_context_fields(&self.event, params_object, true);
        }
        serde_json::json!({
            "action": "send_msg",
            "reply_current": true,
            "params": params,
        })
    }

    fn apply_reply_context(&self, action: &mut serde_json::Value) {
        let reply_current = action
            .get("reply_current")
            .and_then(serde_json::Value::as_bool);
        let Some(params) = action
            .get_mut("params")
            .and_then(|value| value.as_object_mut())
        else {
            return;
        };
        let has_explicit_target = [
            "user_id",
            "group_id",
            "channel_id",
            "guild_id",
            "chat_id",
            "openid",
            "group_openid",
        ]
        .iter()
        .any(|key| params.contains_key(*key));
        if reply_current.unwrap_or(!has_explicit_target) {
            Self::copy_reply_context_fields(&self.event, params, true);
        }
    }

    pub(crate) fn copy_reply_context_fields(
        event: &serde_json::Value,
        params: &mut serde_json::Map<String, serde_json::Value>,
        reply_current: bool,
    ) {
        for key in [
            "message_type",
            "user_id",
            "group_id",
            "channel_id",
            "guild_id",
            "chat_id",
        ] {
            if let Some(value) = event.get(key) {
                params
                    .entry(key.to_string())
                    .or_insert_with(|| value.clone());
            }
        }
        if !reply_current {
            return;
        }
        for key in ["openid", "group_openid", "event_id", "qq_target_type"] {
            if let Some(value) = event.get(key) {
                params
                    .entry(key.to_string())
                    .or_insert_with(|| value.clone());
            }
        }
        if let Some(message_id) = event.get("message_id") {
            params
                .entry("message_id".to_string())
                .or_insert_with(|| message_id.clone());
        }
        if let Some(context) = event.get("reply_context") {
            params
                .entry("reply_context".to_string())
                .or_insert_with(|| context.clone());
        }
    }
}

/// 非 OneBot 平台的出站边界。
///
/// runtime 内部仍生成 OneBot 兼容 action；sink 负责恢复平台原生目标 ID、接受必要的
/// 有损消息段降级，并把调用失败返回给当前阶段。该失败只影响发送结果，不会反向撤销
/// 已产生的插件、数据库或等待会话副作用。
pub use nuono_plugin::RuntimeActionSink;

/// 入站运行时阶段处理器。
#[async_trait::async_trait]
pub trait RuntimeEventStageHandler: Send + Sync {
    fn stage(&self) -> RuntimeEventStage;

    async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()>;
}

/// 阶段化事件处理内核。
pub struct EventKernel {
    legacy_adapter: Arc<dyn PlatformAdapter>,
    wait_delivery: super::wait_delivery::WaitDelivery,
    locales: nuono_app::localization::LocaleService,
    pub agent_runtime: crate::agent::AgentRuntime,
    pub assistant: crate::service::assistant::Assistant,
    /// 等待会话运行时阶段
    wait_session_runtime_stage: Arc<dyn RuntimeEventStageHandler>,
    /// 过滤运行时阶段
    filter_runtime_stage: Arc<dyn RuntimeEventStageHandler>,
    /// 具有独立启停设置的框架指令。
    core_commands_runtime_stage: Arc<dyn RuntimeEventStageHandler>,
    /// 插件宿主运行时阶段
    plugin_host_runtime_stage: Arc<dyn RuntimeEventStageHandler>,
    /// 触发器分发运行时阶段
    trigger_dispatch_runtime_stage: Arc<dyn RuntimeEventStageHandler>,
    /// Matcher 运行时阶段
    matcher_runtime_stage: Arc<dyn RuntimeEventStageHandler>,
    /// Agent Runtime 阶段
    agent_runtime_stage: Arc<dyn RuntimeEventStageHandler>,
    /// 事件发布运行时阶段
    event_publish_runtime_stage: Arc<dyn RuntimeEventStageHandler>,
    /// 数据库
    db: Database,
    /// 权限管理器
    permission_manager: Arc<PermissionManager>,
    /// 插件宿主
    plugin_host: Arc<PluginHost>,
    /// 热更新运行时配置，用于消息日志开关与预览长度。
    config: Arc<crate::config::ConfigManager>,
    business_gate: nuono_app::business_runtime::BusinessGate,
}

/// 运行时管线持有的阶段处理器集合。
pub struct EventKernelPipeline {
    pub filter_handler: Arc<FilterHandler>,
    pub wait_manager: Arc<WaitManager>,
    pub event_handler: Arc<EventHandler>,
    pub matcher_registry: Arc<MatcherRegistry>,
    pub adapter: Arc<dyn PlatformAdapter>,
    pub core_commands: Arc<crate::core_commands::CoreCommands>,
}

impl EventKernel {
    pub(crate) fn wait_registration_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.plugin_host.wait_manager.subscribe_changes()
    }
    /// 当串行执行的业务回调正挂起等待回复时，有界 OneBot 派发器可直接将匹配的回复投递给等待通道，
    /// 并沿用与 WaitSession 阶段相同的安全校验与归属检查。
    pub(crate) async fn try_deliver_waiting_post_event(
        &self,
        bot_id: i64,
        raw_text: &str,
        event: &serde_json::Value,
        route: Option<&super::RuntimeBotRoute>,
    ) -> bool {
        if !self.business_is_running() {
            return false;
        }
        if event.get("post_type").and_then(serde_json::Value::as_str) == Some("interaction") {
            let mut event = event.clone();
            super::runtime_projection::inject_runtime_route(&mut event, route);
            return self.try_deliver_interaction_reply(bot_id, &event).await;
        }
        if event.get("post_type").and_then(serde_json::Value::as_str) != Some("message") {
            return false;
        }
        let mut event = event.clone();
        super::runtime_projection::inject_runtime_route(&mut event, route);
        if self
            .plugin_host
            .wait_manager
            .snapshot_for_event(bot_id, &event)
            .is_none()
        {
            return false;
        }
        let exact_route = route.cloned().unwrap_or_else(|| {
            super::RuntimeBotRoute::new("onebot", bot_id.to_string(), bot_id.to_string())
        });
        let domain = self
            .legacy_adapter
            .parse_incoming(raw_text)
            .ok()
            .and_then(|event| {
                super::runtime_transport::route_identity(&exact_route)
                    .ok()
                    .and_then(|bot| {
                        crate::domain::DomainEventNormalizer::new(bot)
                            .normalize(&event, crate::clock::db_now())
                            .ok()
                    })
            });
        let locale =
            crate::localization::resolve_event_or_default(&self.locales, &event, domain.as_ref())
                .await;
        let consumed = self
            .wait_delivery
            .deliver(bot_id, &event, domain.as_ref(), locale)
            .await;
        if consumed {
            self.record_received_metadata(bot_id, &event, domain.as_ref())
                .await;
            if let Some(event) = domain.as_ref() {
                self.db
                    .record_activity(event, "", nuono_storage::activity::ActivityKind::Message);
            }
            let config = self.config.snapshot().await;
            if config.log.show_receive
                && let Some(summary) = inbound_summary(&event, config.log.message_preview_chars)
            {
                log_receive(&event_bot_id(&event, bot_id), &summary);
            }
        }
        consumed
    }

    /// 在有界平台工作协程抢占常规业务槽位之前，先尝试匹配并投递待处理的回复会话。
    /// 指令按钮与自定义回调仍走常规派发；选项按钮点击绑定在当初注册的等待批次号上，不会误命中后续替换的新等待会话。
    pub async fn try_deliver_waiting_unified_event(
        &self,
        platform: &str,
        event: &UnifiedEvent,
        _sink: Option<Arc<dyn RuntimeActionSink>>,
        route: Option<&super::RuntimeBotRoute>,
    ) -> Result<bool> {
        if !self.business_is_running() {
            return Ok(false);
        }
        let Some(bot_id) = event.self_id() else {
            return Ok(false);
        };
        let runtime_event = super::runtime_projection::unified_event_to_runtime_json_with_route(
            platform, event, route,
        );
        if runtime_event
            .get("post_type")
            .and_then(serde_json::Value::as_str)
            == Some("interaction")
        {
            return Ok(self
                .try_deliver_interaction_reply(bot_id, &runtime_event)
                .await);
        }
        if !event.is_message() {
            return Ok(false);
        }
        let exact_route = route
            .cloned()
            .unwrap_or_else(|| super::RuntimeBotRoute::new(platform, platform, bot_id.to_string()));
        let domain = super::runtime_transport::route_identity(&exact_route)
            .ok()
            .and_then(|bot| {
                crate::domain::DomainEventNormalizer::new(bot)
                    .normalize(event, crate::clock::db_now())
                    .ok()
            });
        let locale = crate::localization::resolve_event_or_default(
            &self.locales,
            &runtime_event,
            domain.as_ref(),
        )
        .await;
        let consumed = self
            .wait_delivery
            .deliver(bot_id, &runtime_event, domain.as_ref(), locale)
            .await;
        if consumed {
            self.record_received_metadata(bot_id, &runtime_event, domain.as_ref())
                .await;
            if let Some(event) = domain.as_ref() {
                self.db
                    .record_activity(event, "", nuono_storage::activity::ActivityKind::Message);
            }
            let config = self.config.snapshot().await;
            if config.log.show_receive
                && let Some(summary) =
                    inbound_summary(&runtime_event, config.log.message_preview_chars)
            {
                log_receive(&event_bot_id(&runtime_event, bot_id), &summary);
            }
        }
        Ok(consumed)
    }

    /// 创建新的 EventKernel
    pub fn new(
        db: Database,
        event_bus: Arc<EventBus>,
        permission_manager: Arc<PermissionManager>,
        plugin_host: Arc<PluginHost>,
        _task_executor: Arc<TaskExecutor>,
        config: Arc<crate::config::ConfigManager>,
        pipeline: EventKernelPipeline,
    ) -> Self {
        let EventKernelPipeline {
            filter_handler,
            wait_manager,
            event_handler,
            matcher_registry,
            adapter,
            core_commands,
        } = pipeline;
        let agent_runtime = plugin_host.bind_runtime(db.clone(), config.clone());
        let wait_delivery = super::wait_delivery::WaitDelivery::new(
            wait_manager.clone(),
            filter_handler.clone(),
            event_handler.clone(),
            plugin_host.clone(),
            core_commands.clone(),
        );
        Self {
            business_gate: nuono_app::business_runtime::BusinessGate::running(),
            legacy_adapter: adapter.clone(),
            wait_delivery: wait_delivery.clone(),
            locales: crate::localization::service(db.clone(), config.clone()),
            agent_runtime: agent_runtime.clone(),
            assistant: crate::service::assistant::Assistant::new(
                db.clone(),
                config.clone(),
                agent_runtime.clone(),
                plugin_host.paths(),
            ),
            wait_session_runtime_stage: Arc::new(
                WaitSessionRuntimeStage::new(wait_manager.clone()).with_admission(wait_delivery),
            ),
            filter_runtime_stage: Arc::new(FilterRuntimeStage::new(filter_handler)),
            core_commands_runtime_stage: Arc::new(CoreCommandsRuntimeStage::new(core_commands)),
            plugin_host_runtime_stage: Arc::new(PluginHostRuntimeStage::new(
                plugin_host.clone(),
                wait_manager,
                permission_manager.clone(),
            )),
            trigger_dispatch_runtime_stage: Arc::new(TriggerDispatchRuntimeStage::new(
                db.clone(),
                event_handler,
            )),
            matcher_runtime_stage: Arc::new(MatcherRuntimeStage::new(matcher_registry, adapter)),
            agent_runtime_stage: Arc::new(AgentRuntimeStage::with_runtime(
                agent_runtime,
                db.clone(),
                config.clone(),
            )),
            event_publish_runtime_stage: Arc::new(EventPublishRuntimeStage::new(
                db.clone(),
                event_bus,
            )),
            db,
            permission_manager,
            plugin_host,
            config,
        }
    }

    pub(crate) fn with_business_gate(
        mut self,
        gate: nuono_app::business_runtime::BusinessGate,
    ) -> Self {
        self.business_gate = gate;
        self
    }

    /// 平台适配器与所有入站入口共用的业务运行许可。
    pub fn business_is_running(&self) -> bool {
        self.business_gate.is_running()
    }

    /// 返回供平台运行时注册主动发送能力的进程级路由器。
    pub fn action_router(&self) -> Arc<super::RuntimeActionRouter> {
        self.plugin_host.action_router()
    }

    pub async fn send_agent_run_reply(
        &self,
        reply: &nuono_app::agent::run::AgentRunReply,
    ) -> Result<()> {
        use crate::domain::{DeliveryTarget, MessageSegment, OutboundAction};
        if let Some(native) = &reply.context.native {
            let target = match &native.conversation {
                Some(conversation) => DeliveryTarget::Conversation {
                    conversation: conversation.clone(),
                },
                None => DeliveryTarget::Actor {
                    actor: native.actor.clone(),
                },
            };
            let action = OutboundAction::SendMessage {
                target,
                segments: vec![MessageSegment::Text {
                    text: reply.content.clone(),
                }],
                reply_to: reply
                    .context
                    .message_id
                    .as_deref()
                    .and_then(|id| crate::domain::NativeId::new(id).ok()),
            };
            self.action_router()
                .notify_typed(
                    nuono_app::transport::TransportAction::new(native.bot.clone(), action)
                        .with_authority(nuono_app::action::ActionAuthority::Runtime {
                            stage: "AgentRuntimeResume".into(),
                        }),
                )
                .await
        } else {
            let route = self
                .action_router()
                .connected_routes()
                .into_iter()
                .find(|route| {
                    route.platform == reply.context.platform
                        && route.bot_self_id == reply.context.bot_id.to_string()
                })
                .ok_or_else(|| {
                    crate::error::NuonoError::Connection("Agent 回复目标已断开".into())
                })?;
            let params = match reply.context.group_id {
                Some(group_id) => {
                    serde_json::json!({"message_type":"group","group_id":group_id,"message":reply.content})
                }
                None => {
                    serde_json::json!({"message_type":"private","user_id":reply.context.user_id,"message":reply.content})
                }
            };
            self.action_router()
                .notify_action(&route, "send_msg", params)
                .await
        }
    }

    /// 处理真实入站 OneBot WebSocket 事件。
    ///
    /// `ConnectionManager` 已在调用前截走带 `echo` 的 API 回包，因此这里面对的是带
    /// `post_type` 的上报事件。缺少 `post_type` 时按非事件静默忽略，不进入任何阶段。
    pub(crate) fn retain_queued_receipt(
        &self,
        received_at: std::time::Instant,
    ) -> super::plugin_host::trigger_control::QueuedReceipt {
        self.plugin_host.retain_queued_receipt(received_at)
    }

    pub async fn process_runtime_post_event(
        &self,
        bot_id: i64,
        raw_text: &str,
        event: serde_json::Value,
        bot_sender: mpsc::Sender<TransportFrame>,
    ) -> Result<RuntimeEventProcessResult> {
        self.process_runtime_post_event_with_route(bot_id, raw_text, event, bot_sender, None)
            .await
    }

    pub async fn process_runtime_post_event_with_route(
        &self,
        bot_id: i64,
        raw_text: &str,
        mut event: serde_json::Value,
        bot_sender: mpsc::Sender<TransportFrame>,
        route: Option<&super::RuntimeBotRoute>,
    ) -> Result<RuntimeEventProcessResult> {
        let Some(post_type) = event.get("post_type").and_then(|p| p.as_str()) else {
            return Ok(RuntimeEventProcessResult::default());
        };
        let is_message = post_type == "message";
        super::runtime_projection::inject_runtime_route(&mut event, route);

        let exact_route = route.cloned().unwrap_or_else(|| {
            super::RuntimeBotRoute::new("onebot", bot_id.to_string(), bot_id.to_string())
        });
        let domain_event = self
            .legacy_adapter
            .parse_incoming(raw_text)
            .ok()
            .and_then(|event| {
                super::runtime_transport::route_identity(&exact_route)
                    .ok()
                    .and_then(|bot| {
                        crate::domain::DomainEventNormalizer::new(bot)
                            .normalize(&event, crate::clock::db_now())
                            .ok()
                    })
            });
        let action_transport = super::runtime_transport::SinkTransport::new(
            &exact_route,
            0,
            Arc::new(super::runtime_transport::OneBotChannelSink(
                bot_sender.clone(),
            )),
        )
        .map(|transport| {
            Arc::new(
                transport
                    .with_audit(Arc::new(self.db.clone()))
                    .with_message_policy_source(self.action_router().message_policy_source()),
            ) as Arc<dyn ActionTransport>
        })
        .ok();
        let mut ctx = RuntimeEventContext {
            locale: nuono_app::localization::LocaleContext::default(),
            bot_id,
            raw_text,
            event,
            bot_sender: Some(bot_sender),
            action_sink: None,
            action_transport,
            domain_event,
            unified_event: None,
            resolved_user_id: None,
            resolved_identity_id: None,
            web_chat_account_id: None,
            blocked: false,
            is_message,
            reply_attempts: std::sync::atomic::AtomicUsize::new(0),
            reply_ok: std::sync::atomic::AtomicUsize::new(0),
        };

        Ok(self.process_runtime_context(&mut ctx).await)
    }

    /// 处理协议无关的入站事件。
    ///
    /// 当前阶段先把 `UnifiedEvent` 有损投影为兼容现有插件/触发器的 OneBot 形状 JSON，
    /// 并复用同一套 runtime stage。无 sink 的调用仍会运行处理逻辑，但任何回复只能被
    /// 记录为发送失败；生产跨平台入口应使用 [`Self::process_unified_event_with_sink`]。
    pub async fn process_unified_event(
        &self,
        platform: &str,
        event: UnifiedEvent,
    ) -> Result<RuntimeEventProcessResult> {
        self.process_unified_event_with_sink(platform, event, None)
            .await
    }

    pub async fn process_unified_event_with_sink(
        &self,
        platform: &str,
        event: UnifiedEvent,
        action_sink: Option<Arc<dyn RuntimeActionSink>>,
    ) -> Result<RuntimeEventProcessResult> {
        self.process_unified_event_with_route(platform, event, action_sink, None)
            .await
    }

    pub async fn process_unified_event_with_route(
        &self,
        platform: &str,
        event: UnifiedEvent,
        action_sink: Option<Arc<dyn RuntimeActionSink>>,
        route: Option<&super::RuntimeBotRoute>,
    ) -> Result<RuntimeEventProcessResult> {
        self.process_unified_event_inner(platform, event, action_sink, route, None)
            .await
    }

    /// 处理已登录账号的网页对话事件：复用完整插件与核心指令管线，透传服务端核验账号角色，
    /// 未命中指令或插件时转入该账号的个人伴侣助手（角色卡、跨端记忆与额度控制）。
    pub async fn process_web_chat_event(
        &self,
        account_id: i64,
        event: UnifiedEvent,
        action_sink: Option<Arc<dyn RuntimeActionSink>>,
        route: Option<&super::RuntimeBotRoute>,
    ) -> Result<RuntimeEventProcessResult> {
        let is_active = self
            .db
            .find_user_by_id(account_id)
            .await?
            .is_some_and(|user| user.status == crate::db::STATUS_ACTIVE);
        if !is_active {
            return Ok(RuntimeEventProcessResult {
                blocked: true,
                is_message: event.is_message(),
            });
        }
        self.process_unified_event_inner("onebot", event, action_sink, route, Some(account_id))
            .await
    }

    async fn process_unified_event_inner(
        &self,
        platform: &str,
        event: UnifiedEvent,
        action_sink: Option<Arc<dyn RuntimeActionSink>>,
        route: Option<&super::RuntimeBotRoute>,
        web_chat_account_id: Option<i64>,
    ) -> Result<RuntimeEventProcessResult> {
        let Some(bot_id) = event.self_id() else {
            return Ok(RuntimeEventProcessResult::default());
        };
        let runtime_event = super::runtime_projection::unified_event_to_runtime_json_with_route(
            platform, &event, route,
        );
        let raw_text = serde_json::to_string(&runtime_event)?;
        let is_message = event.is_message();

        let route = route.cloned().unwrap_or_else(|| {
            super::RuntimeBotRoute::new(
                platform,
                platform,
                event.self_id().unwrap_or(0).to_string(),
            )
        });
        let domain_event = super::runtime_transport::route_identity(&route)
            .ok()
            .and_then(|bot| {
                crate::domain::DomainEventNormalizer::new(bot)
                    .normalize(&event, crate::clock::db_now())
                    .ok()
            });
        let action_transport = action_sink
            .clone()
            .and_then(|sink| super::runtime_transport::SinkTransport::new(&route, 0, sink).ok())
            .map(|transport| {
                Arc::new(
                    transport
                        .with_audit(Arc::new(self.db.clone()))
                        .with_message_policy_source(self.action_router().message_policy_source()),
                ) as Arc<dyn ActionTransport>
            });
        let mut ctx = RuntimeEventContext {
            locale: nuono_app::localization::LocaleContext::default(),
            bot_id,
            raw_text: &raw_text,
            event: runtime_event,
            bot_sender: None,
            action_sink,
            action_transport,
            domain_event,
            unified_event: Some(event),
            resolved_user_id: web_chat_account_id,
            resolved_identity_id: None,
            web_chat_account_id,
            blocked: false,
            is_message,
            reply_attempts: std::sync::atomic::AtomicUsize::new(0),
            reply_ok: std::sync::atomic::AtomicUsize::new(0),
        };

        let result = match web_chat_account_id {
            Some(account_id) => {
                nuono_plugin::permission::with_authenticated_account(
                    account_id,
                    self.process_runtime_context(&mut ctx),
                )
                .await
            }
            None => self.process_runtime_context(&mut ctx).await,
        };
        Ok(result)
    }

    /// 处理以字符串原生身份为权威的领域事件。
    ///
    /// 旧插件仍接收显式兼容投影；typed transport 只在可转换的回复/action 边界使用。
    pub async fn process_domain_event_with_transport(
        &self,
        event: DomainEvent,
        action_transport: Arc<dyn ActionTransport>,
    ) -> Result<RuntimeEventProcessResult> {
        let runtime_event = crate::domain::project_legacy_runtime_event(
            &event,
            &format!(
                "{}_{}",
                action_transport.protocol().family,
                action_transport.protocol().version
            ),
        );
        let raw_text = serde_json::to_string(&runtime_event)?;
        let is_message = event.message().is_some();
        let bot_id = event
            .bot
            .account_id
            .value()
            .as_str()
            .parse::<i64>()
            .unwrap_or(0);
        let action_transport: Arc<dyn ActionTransport> = Arc::new(LoggingActionTransport::new(
            action_transport,
            self.config.clone(),
        ));
        let mut ctx = RuntimeEventContext {
            locale: nuono_app::localization::LocaleContext::default(),
            bot_id,
            raw_text: &raw_text,
            event: runtime_event,
            bot_sender: None,
            action_sink: None,
            action_transport: Some(action_transport),
            domain_event: Some(event),
            unified_event: None,
            resolved_user_id: None,
            resolved_identity_id: None,
            web_chat_account_id: None,
            blocked: false,
            is_message,
            reply_attempts: std::sync::atomic::AtomicUsize::new(0),
            reply_ok: std::sync::atomic::AtomicUsize::new(0),
        };
        Ok(self.process_runtime_context(&mut ctx).await)
    }

    async fn process_runtime_context(
        &self,
        ctx: &mut RuntimeEventContext<'_>,
    ) -> RuntimeEventProcessResult {
        if !self.business_is_running() {
            return RuntimeEventProcessResult {
                blocked: true,
                is_message: ctx.is_message,
            };
        }
        if ctx
            .event
            .get("post_type")
            .and_then(serde_json::Value::as_str)
            == Some("notice")
        {
            self.plugin_host.invalidate_platform_role_proofs();
        }
        if let Some(domain_event) = ctx.domain_event.as_ref()
            && let Some(conversation) = domain_event.conversation.as_ref()
        {
            let hinted_name = ctx
                .event
                .get("group_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let bot_platform = domain_event.bot.account_id.platform().as_str();
            let bot_id = domain_event.bot.account_id.value().as_str();
            match self
                .db
                .is_conversation_enabled_with_context(
                    conversation,
                    hinted_name,
                    Some((bot_platform, bot_id)),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return RuntimeEventProcessResult {
                        blocked: true,
                        is_message: ctx.is_message,
                    };
                }
                Err(error) => {
                    warn!(error_kind = error.kind(), "群服务状态不可用，拒绝分发");
                    return RuntimeEventProcessResult {
                        blocked: true,
                        is_message: ctx.is_message,
                    };
                }
            }
        }
        let scope = match self
            .plugin_host
            .event_configuration_scope(ctx.domain_event.as_ref())
            .await
        {
            Ok(scope) => scope,
            Err(error) => {
                warn!(error_kind = error.kind(), "事件配置对象不可用，拒绝分发");
                return RuntimeEventProcessResult {
                    blocked: true,
                    is_message: ctx.is_message,
                };
            }
        };
        nuono_plugin::config::with_runtime_configurations(
            scope,
            self.process_runtime_context_scoped(ctx),
        )
        .await
    }

    async fn process_runtime_context_scoped(
        &self,
        ctx: &mut RuntimeEventContext<'_>,
    ) -> RuntimeEventProcessResult {
        if ctx.is_message
            && let Some(event) = ctx.domain_event.as_ref()
        {
            self.db
                .record_activity(event, "", nuono_storage::activity::ActivityKind::Message);
        }
        let command_owner = if ctx
            .event
            .get("post_type")
            .and_then(serde_json::Value::as_str)
            == Some("interaction")
        {
            if !self.ensure_interactive_user_resolved(ctx).await {
                return RuntimeEventProcessResult {
                    blocked: true,
                    is_message: ctx.is_message,
                };
            }
            match self.dispatch_runtime_interaction(ctx).await {
                interaction_runtime::InteractionDispatch::Command(plugin, epoch) => {
                    Some((plugin, epoch))
                }
                interaction_runtime::InteractionDispatch::Finished(result) => return result,
            }
        } else {
            None
        };
        ctx.locale = crate::localization::resolve_event_or_default(
            &self.locales,
            &ctx.event,
            ctx.domain_event.as_ref(),
        )
        .await;
        if ctx.is_message {
            let config = self.config.snapshot().await;
            let bot_id = event_bot_id(&ctx.event, ctx.bot_id);
            if config.log.show_receive
                && let Some(summary) = inbound_summary(&ctx.event, config.log.message_preview_chars)
            {
                log_receive(&bot_id, &summary);
            }
            if config.log.show_send
                && let Some(inner) = ctx.action_sink.take()
            {
                let platform = ctx
                    .event
                    .get("platform")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                ctx.action_sink = Some(Arc::new(LoggingRuntimeActionSink::new(
                    inner,
                    self.config.clone(),
                    platform,
                    bot_id,
                )));
            }
        }

        let pipeline = RuntimeEventPipeline::default();
        super::interaction::with_command_owner(
            command_owner,
            super::plugin_host::trigger_control::with_message_admission(
                nuono_app::pipeline::process_event(self, &pipeline, ctx, |stage, _| {
                    if stage == RuntimeEventStage::TriggerDispatch {
                        super::plugin_host::trigger_control::finish_message_admission();
                    }
                }),
            ),
        )
        .await;

        // EventPublish 阶段维持原有行为，仅持久化未被消费的消息。
        // 此处仅利用路由元数据为已被消费或拦截的消息补计一次入站活跃度，不会将消息正文落库。
        if ctx.is_message && ctx.blocked {
            self.record_received_metadata(ctx.bot_id, &ctx.event, ctx.domain_event.as_ref())
                .await;
        }

        RuntimeEventProcessResult {
            blocked: ctx.blocked,
            is_message: ctx.is_message,
        }
    }

    async fn record_received_metadata(
        &self,
        legacy_bot_id: i64,
        event: &serde_json::Value,
        domain: Option<&DomainEvent>,
    ) {
        let bot_id = domain
            .map(|event| event.bot.account_id.value().as_str().to_owned())
            .unwrap_or_else(|| legacy_bot_id.to_string());
        let mut metadata = serde_json::json!({"message":"", "raw_message":""});
        if let Some(domain_event) = domain {
            metadata["platform"] = serde_json::json!(domain_event.bot.platform().as_str());
            let instance_id = domain_event.bot.instance_id.as_str();
            if !instance_id.is_empty() {
                metadata["bot_instance_id"] = serde_json::json!(instance_id);
            }
        }
        for key in [
            "platform",
            "message_type",
            "message_id",
            "user_id",
            "group_id",
        ] {
            if metadata.get(key).is_none()
                && let Some(value) = event.get(key)
            {
                metadata[key] = value.clone();
            }
        }
        // 在共享的尽力而为（best-effort）统计写入队列中，按具体丢弃原因记一次入站丢弃计数。
        let _ = self
            .db
            .save_message_history(&bot_id, &metadata, "inbound")
            .await;
    }

    fn is_private_message(event: &serde_json::Value) -> bool {
        event
            .get("message_type")
            .and_then(serde_json::Value::as_str)
            == Some("private")
    }

    /// 群聊场景仅在显式 `@机器人` 或 `/ai` 前缀触发 AI 交互时才进行占位用户懒创建，避免被动闲聊刷爆用户表。
    fn is_group_ai_trigger(ctx: &RuntimeEventContext<'_>) -> bool {
        let raw = ctx
            .event
            .get("raw_message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        let lower = raw.to_ascii_lowercase();
        if lower == "/ai" || lower.starts_with("/ai ") {
            return true;
        }
        let self_id = ctx
            .domain_event
            .as_ref()
            .map(|e| e.bot.account_id.value().as_str().to_owned())
            .unwrap_or_else(|| ctx.bot_id.to_string());
        if raw.contains(&format!("[CQ:at,qq={self_id}]")) {
            return true;
        }
        if let Some(domain_event) = ctx.domain_event.as_ref()
            && let Some(message) = domain_event.message()
            && message.segments.iter().any(|seg| {
                matches!(
                    seg,
                    crate::domain::MessageSegment::At { user_id }
                        if user_id == domain_event.bot.account_id.value().as_str()
                )
            })
        {
            return true;
        }
        ctx.event
            .get("message")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|segments| {
                segments.iter().any(|seg| {
                    seg.get("type").and_then(serde_json::Value::as_str) == Some("at")
                        && seg
                            .get("data")
                            .and_then(|d| d.get("qq"))
                            .and_then(|qq| {
                                qq.as_str()
                                    .map(ToOwned::to_owned)
                                    .or_else(|| qq.as_i64().map(|id| id.to_string()))
                            })
                            .as_deref()
                            == Some(self_id.as_str())
                })
            })
    }

    fn looks_like_core_or_explicit_command(raw_message: &str) -> bool {
        let trimmed = raw_message.trim();
        if trimmed.is_empty() {
            return false;
        }
        let lower = trimmed.to_ascii_lowercase();
        lower.starts_with("nuono")
            || trimmed.starts_with('/')
            || trimmed.starts_with('!')
            || trimmed.starts_with('！')
            || trimmed.starts_with('#')
            || trimmed.starts_with('.')
            || trimmed.starts_with('。')
    }

    /// 过滤机器人自身回环、匿名发言、系统广播等非真人事件，防止污染占位用户表。
    fn is_non_human_event(ctx: &RuntimeEventContext<'_>) -> bool {
        let post_type = ctx
            .event
            .get("post_type")
            .and_then(serde_json::Value::as_str);
        if !matches!(post_type, Some("message" | "interaction")) {
            return true;
        }
        if ctx
            .event
            .get("anonymous")
            .is_some_and(|value| !value.is_null())
        {
            return true;
        }
        if let Some(domain_event) = ctx.domain_event.as_ref() {
            let sender = domain_event
                .actor
                .as_ref()
                .or_else(|| domain_event.message().map(|m| &m.sender));
            let Some(sender) = sender else {
                return true;
            };
            if sender.value() == domain_event.bot.account_id.value()
                && sender.platform() == domain_event.bot.account_id.platform()
            {
                return true;
            }
            return nuono_storage::identity_resolver::is_non_human_identity(sender);
        }
        let sender_id = ctx.event.get("user_id").and_then(|value| {
            value
                .as_i64()
                .map(|id| id.to_string())
                .or_else(|| value.as_str().map(str::trim).map(ToOwned::to_owned))
        });
        let Some(sender_id) = sender_id.filter(|id| !id.is_empty() && id != "0") else {
            return true;
        };
        let self_id = ctx
            .event
            .get("self_id")
            .and_then(|value| {
                value
                    .as_i64()
                    .map(|id| id.to_string())
                    .or_else(|| value.as_str().map(str::trim).map(ToOwned::to_owned))
            })
            .unwrap_or_else(|| ctx.bot_id.to_string());
        sender_id == self_id
    }

    fn extract_event_sender(
        ctx: &RuntimeEventContext<'_>,
    ) -> Option<(nuono_domain::IdentityRef, Option<String>)> {
        let nickname = ctx
            .event
            .get("sender")
            .and_then(|sender| sender.get("nickname"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned);
        if let Some(domain_event) = ctx.domain_event.as_ref() {
            let sender = domain_event
                .actor
                .clone()
                .or_else(|| domain_event.message().map(|m| m.sender.clone()))?;
            return Some((sender, nickname));
        }
        let user_id = ctx.event.get("user_id").and_then(|value| {
            value
                .as_i64()
                .filter(|id| *id > 0)
                .map(|id| id.to_string())
                .or_else(|| {
                    value
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != "0")
                        .map(ToOwned::to_owned)
                })
        })?;
        let platform = match ctx
            .event
            .get("platform")
            .and_then(serde_json::Value::as_str)
        {
            Some("telegram") => nuono_domain::Platform::Telegram,
            Some("discord") => nuono_domain::Platform::Discord,
            Some("feishu") => nuono_domain::Platform::Feishu,
            Some("qq_official") => nuono_domain::Platform::QqOfficial,
            Some("onebot") | None => nuono_domain::Platform::OneBot,
            Some(other) => nuono_domain::Platform::Other(other.to_owned()),
        };
        let identity = nuono_domain::IdentityRef::new(
            platform,
            nuono_domain::IdentityKind::User,
            nuono_domain::IdentityScheme::Native,
            user_id,
            None::<String>,
        )
        .ok()?;
        Some((identity, nickname))
    }

    /// 交互懒创建中间件：仅在确认触发交互（指令、私聊、按钮或 AI）时解析或创建占位用户，
    /// 并向 `ctx` 注入统一的 `resolved_user_id`；若该占位或正式账号已被封禁则返回 `false` 阻断后续分发。
    async fn ensure_interactive_user_resolved(&self, ctx: &mut RuntimeEventContext<'_>) -> bool {
        if ctx.resolved_user_id.is_some() {
            return true;
        }
        if Self::is_non_human_event(ctx) {
            return true;
        }
        let Some((sender, nickname)) = Self::extract_event_sender(ctx) else {
            return true;
        };
        match self
            .db
            .resolve_or_provision_identity(&sender, nickname.as_deref())
            .await
        {
            Ok(resolved) => {
                ctx.resolved_user_id = Some(resolved.user_id);
                ctx.resolved_identity_id = Some(resolved.identity_id);
                if let Ok(Some(user)) = self.db.find_user_by_id(resolved.user_id).await
                    && user.status != "active"
                {
                    ctx.blocked = true;
                    return false;
                }
                true
            }
            Err(error) => {
                warn!(bot_id = %ctx.bot_id, error = %error, "交互身份懒创建失败");
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::{MessageType, UnifiedMessage, UnifiedSegment};
    use crate::core::runtime_outbound::send_runtime_action;
    use crate::core::runtime_projection;
    use std::sync::Mutex;

    struct RecordingRuntimeStage {
        stage: RuntimeEventStage,
        entered: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl RuntimeEventStageHandler for RecordingRuntimeStage {
        fn stage(&self) -> RuntimeEventStage {
            self.stage
        }

        async fn handle(&self, _: &mut RuntimeEventContext<'_>) -> Result<()> {
            self.entered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn framework_agent_prefix_reaches_agent_with_enabled_or_disabled_slash_plugin() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = std::env::temp_dir().join(format!("nuono-agent-route-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let db = crate::test_support::test_db(1).await;
        let config = Arc::new(
            crate::config::ConfigManager::load(Some(root.join("nuono.toml")))
                .await
                .unwrap(),
        );
        let event_bus = Arc::new(EventBus::new());
        let permissions = Arc::new(PermissionManager::new(db.clone()));
        let waits = Arc::new(WaitManager::new());
        let host = PluginHost::new(
            db.clone(),
            event_bus.clone(),
            permissions.clone(),
            crate::core::Paths::with_root(&root),
            waits.clone(),
            crate::core::FrameworkControl::new(),
            Arc::new(crate::config::PluginConfigManager::new(
                root.join("plugins"),
            )),
        )
        .await
        .unwrap();
        let mut handler = EventHandler::new(
            db.clone(),
            crate::core::CommandDispatchServices {
                trigger_registry: host.trigger_registry(),
                cooldown: host.cooldown_tracker.clone(),
                stats: host.command_stats.clone(),
            },
            permissions.clone(),
            event_bus.clone(),
            waits.clone(),
            Arc::new(crate::core::ApiClient::new()),
        );
        handler.set_plugin_host(host.clone());
        let pipeline = EventKernelPipeline {
            filter_handler: Arc::new(FilterHandler::new(db.clone())),
            wait_manager: waits.clone(),
            event_handler: Arc::new(handler),
            matcher_registry: Arc::new(MatcherRegistry::new(Arc::new(
                crate::core::SessionManager::new(),
            ))),
            adapter: Arc::new(nuono_adapter_fixture::FixtureAdapter),
            core_commands: Arc::new(crate::core_commands::CoreCommands::new(
                config.clone(),
                permissions.clone(),
                crate::SystemMonitor::new(),
            )),
        };
        let mut kernel = EventKernel::new(
            db.clone(),
            event_bus,
            permissions,
            host.clone(),
            Arc::new(TaskExecutor::new(1)),
            config,
            pipeline,
        );
        let agent_entered = Arc::new(AtomicUsize::new(0));
        let matcher_entered = Arc::new(AtomicUsize::new(0));
        // 只替换最终执行器计数，保留生产管线的路由、Filter、等待及核心入口；不调用模型。
        kernel.agent_runtime_stage = Arc::new(RecordingRuntimeStage {
            stage: RuntimeEventStage::AgentRuntime,
            entered: agent_entered.clone(),
        });
        kernel.matcher_runtime_stage = Arc::new(RecordingRuntimeStage {
            stage: RuntimeEventStage::Matcher,
            entered: matcher_entered.clone(),
        });
        for enabled in [true, false] {
            let mut slash = crate::core::Plugins::new("SlashBridge").with_prefix(vec!["/".into()]);
            slash.enabled = enabled;
            host.register_plugin_config(slash).await;
            let event = serde_json::json!({
                "post_type":"message", "message_type":"private", "sub_type":"friend",
                "time":1, "self_id":42, "user_id":7, "message_id":1,
                "raw_message":"/ai 问题", "message":[{"type":"text","data":{"text":"/ai 问题"}}],
                "sender":{"user_id":7,"nickname":"tester"}
            });
            let session = crate::onebot::build_session_id(42, &event);
            let mut wait = waits.register_wait(session.clone(), std::time::Duration::from_secs(30));
            let (sender, _replies) = mpsc::channel(4);
            let result = kernel
                .process_runtime_post_event(42, &event.to_string(), event, sender)
                .await
                .unwrap();
            assert!(!result.blocked, "保留入口本身不冒充 block");
            assert!(matches!(
                wait.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            waits.cancel(&session);
        }
        assert_eq!(agent_entered.load(Ordering::SeqCst), 2);
        assert_eq!(matcher_entered.load(Ordering::SeqCst), 0);
        // 每次入口都只计数一次，即使核心入口绕过了插件分发。
        let activity = db.activity_report(Default::default()).await.unwrap();
        assert_eq!(activity.totals.messages, 2);
        assert_eq!(activity.totals.plugin_calls, 0);

        // 交互懒创建核验：同一私聊用户两次交互仅生成 1 条占位账号；群聊被动闲聊与机器人自回环绝不建号。
        let count_users = || async {
            use sea_orm::{ConnectionTrait, Statement};
            db.orm()
                .query_one_raw(Statement::from_string(
                    db.backend(),
                    "SELECT COUNT(*) AS n FROM nuono_users",
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<i64>("", "n")
                .unwrap()
        };
        assert_eq!(count_users().await, 1);

        let group_chatter = serde_json::json!({
            "post_type": "message",
            "message_type": "group",
            "sub_type": "normal",
            "time": 2,
            "self_id": 42,
            "group_id": 99999,
            "user_id": 88888,
            "message_id": 2,
            "raw_message": "今天天气真不错",
            "message": [{"type": "text", "data": {"text": "今天天气真不错"}}],
            "sender": {"user_id": 88888, "nickname": "路人甲"}
        });
        let (tx_group, _) = mpsc::channel(4);
        kernel
            .process_runtime_post_event(42, &group_chatter.to_string(), group_chatter, tx_group)
            .await
            .unwrap();
        assert_eq!(count_users().await, 1, "群聊被动闲聊绝不能触发占位用户创建");

        let bot_self_echo = serde_json::json!({
            "post_type": "message",
            "message_type": "private",
            "sub_type": "friend",
            "time": 3,
            "self_id": 42,
            "user_id": 42,
            "message_id": 3,
            "raw_message": "/ai 自言自语",
            "message": [{"type": "text", "data": {"text": "/ai 自言自语"}}],
            "sender": {"user_id": 42, "nickname": "Bot"}
        });
        let (tx_self, _) = mpsc::channel(4);
        kernel
            .process_runtime_post_event(42, &bot_self_echo.to_string(), bot_self_echo, tx_self)
            .await
            .unwrap();
        assert_eq!(
            count_users().await,
            1,
            "机器人自身回环事件绝不能创建占位用户"
        );
        assert_eq!(activity.dau, 1);
        drop(kernel);
        drop(host);
        let _ = db.shutdown().await;
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    struct RecordingActionSink {
        actions: Mutex<Vec<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl RuntimeActionSink for RecordingActionSink {
        async fn send_action(&self, action: serde_json::Value) -> Result<()> {
            self.actions.lock().unwrap().push(action);
            Ok(())
        }
    }

    #[tokio::test]
    async fn wait_reply_callback_uses_non_onebot_action_sink() {
        let sink = Arc::new(RecordingActionSink {
            actions: Mutex::new(Vec::new()),
        });
        let action = serde_json::json!({
            "action": "send_msg",
            "params": {"message": "second reply"},
        });
        let action_sink: Arc<dyn RuntimeActionSink> = sink.clone();

        assert!(
            send_runtime_action(
                42,
                None,
                Some(&action_sink),
                None,
                None,
                action.clone(),
                "PluginHostWaitReply",
            )
            .await
        );
        assert_eq!(sink.actions.lock().unwrap().as_slice(), &[action]);
    }

    #[test]
    fn explicit_target_is_not_overwritten_by_reply_context() {
        let event = serde_json::json!({
            "message_type": "group",
            "user_id": "user-a",
            "group_id": "group-a",
            "openid": "user-a",
            "group_openid": "group-a",
            "message_id": "message-a",
            "event_id": "event-a"
        });
        let mut action = serde_json::json!({
            "action": "send_private_msg",
            "params": {
                "user_id": "user-b",
                "message": "private"
            }
        });
        let context = RuntimeEventContext {
            locale: nuono_app::localization::LocaleContext::default(),
            bot_id: 1,
            raw_text: "",
            event,
            bot_sender: None,
            action_sink: None,
            action_transport: None,
            domain_event: None,
            unified_event: None,
            resolved_user_id: None,
            resolved_identity_id: None,
            web_chat_account_id: None,
            blocked: false,
            is_message: true,
            reply_attempts: std::sync::atomic::AtomicUsize::new(0),
            reply_ok: std::sync::atomic::AtomicUsize::new(0),
        };
        context.apply_reply_context(&mut action);
        assert_eq!(action["params"]["user_id"], "user-b");
        assert!(action["params"].get("openid").is_none());
        assert!(action["params"].get("group_openid").is_none());
        assert!(action["params"].get("message_id").is_none());
    }

    #[test]
    fn qq_runtime_event_preserves_native_targets_and_numeric_bot_id() {
        let event = UnifiedEvent::Message(UnifiedMessage {
            message_id: "msg-1".to_string(),
            self_id: 123456,
            user_id: 11,
            group_id: Some(22),
            content: "hello".to_string(),
            message_type: MessageType::Group,
            sub_type: "normal".to_string(),
            sender: crate::core::platform::SenderInfo {
                user_id: "member-openid".to_string(),
                nickname: None,
                card: None,
                role: None,
            },
            time: 1,
            raw: serde_json::json!({
                "app_id": "123456",
                "openid": "member-openid",
                "group_openid": "group-openid",
                "event_id": "event-1",
                "qq_target_type": "group"
            }),
            segments: vec![UnifiedSegment::Text {
                text: "hello".to_string(),
            }],
        });

        let runtime = runtime_projection::unified_event_to_runtime_json("qq_official", &event);
        assert_eq!(runtime["self_id"], 123456);
        assert_eq!(runtime["user_id"], "member-openid");
        assert_eq!(runtime["group_id"], "group-openid");
        assert_eq!(runtime["event_id"], "event-1");
        assert_eq!(runtime["reply_context"]["openid"], "member-openid");
        assert_eq!(runtime["reply_context"]["group_openid"], "group-openid");
    }
}
