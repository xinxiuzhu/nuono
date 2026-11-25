//! 生产入站链路的 runtime 阶段实现。
//!
//! 阶段顺序由 [`super::event_kernel::RuntimeEventPipeline`] 定义；每个阶段通过
//! `RuntimeEventContext.blocked` 表达消费语义。

use crate::agent::{AgentRuntime, ToolRegistry};
use crate::api::waiter::WaitManager;
use crate::core::{
    event_bus::{Event, EventBus, EventType},
    filter::{FilterHandler, FilterResponse},
    handler::EventHandler,
    matcher::MatcherRegistry,
    permission::PermissionManager,
    platform::{PlatformAdapter, UnifiedEvent},
    plugin_host::{PluginFilterContext, PluginHost},
};
use crate::db::Database;
use crate::error::Result;
use crate::plugin::PluginResponse;
use std::sync::Arc;
use tracing::{debug, trace, warn};

use super::event_kernel::{RuntimeEventContext, RuntimeEventStage, RuntimeEventStageHandler};
use super::runtime_outbound::{runtime_identity, send_runtime_action};

/// 等待会话拦截阶段。
///
/// 插件发起 `WaitReply` 后，下一条属于同一会话的消息应直接回到等待中的
/// 插件回调，而不是继续进入普通消息触发链。
pub struct WaitSessionRuntimeStage {
    wait_manager: Arc<WaitManager>,
    admission: Option<super::wait_delivery::WaitDelivery>,
}

impl WaitSessionRuntimeStage {
    pub fn new(wait_manager: Arc<WaitManager>) -> Self {
        Self {
            wait_manager,
            admission: None,
        }
    }
    pub(crate) fn with_admission(mut self, admission: super::wait_delivery::WaitDelivery) -> Self {
        self.admission = Some(admission);
        self
    }
}

#[async_trait::async_trait]
impl RuntimeEventStageHandler for WaitSessionRuntimeStage {
    fn stage(&self) -> RuntimeEventStage {
        RuntimeEventStage::WaitSession
    }

    async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()> {
        if !ctx.is_message {
            return Ok(());
        }
        if let Some(admission) = &self.admission {
            ctx.blocked |= admission
                .deliver(
                    ctx.bot_id,
                    &ctx.event,
                    ctx.domain_event.as_ref(),
                    ctx.locale,
                )
                .await;
            return Ok(());
        }
        // 系统前缀由核心入口处理，不能成为插件等待会话的回复；仍继续经过 Filter。
        if ctx.is_message
            && ctx
                .event
                .get("raw_message")
                .and_then(serde_json::Value::as_str)
                .is_some_and(crate::core_commands::is_core_command)
        {
            return Ok(());
        }
        let Some(wait) = self.wait_manager.snapshot_for_event(ctx.bot_id, &ctx.event) else {
            return Ok(());
        };
        if self.wait_manager.respond_if_current(
            &wait,
            nuono_plugin::api::waiter::WaitEvent {
                raw: ctx.event.clone(),
                domain: ctx.domain_event.clone(),
                locale: ctx.locale,
            },
        ) {
            ctx.blocked = true;
            trace!(session_id = %wait.session_id, stage = "WaitManager", "消息已交给等待会话，停止后续分发");
        }
        Ok(())
    }
}

/// 全局过滤阶段。
///
/// 命中黑名单、禁用群、白名单限制或过滤规则时阻断；安全配置或数据库读取失败时
/// 同样阻断，避免运行时统一的 fail-open 策略绕过访问控制。
pub struct FilterRuntimeStage {
    filter_handler: Arc<FilterHandler>,
}

impl FilterRuntimeStage {
    pub fn new(filter_handler: Arc<FilterHandler>) -> Self {
        Self { filter_handler }
    }
}

#[async_trait::async_trait]
impl RuntimeEventStageHandler for FilterRuntimeStage {
    fn stage(&self) -> RuntimeEventStage {
        RuntimeEventStage::Filter
    }

    async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()> {
        match self.filter_handler.handle_event(&ctx.event).await {
            Ok(FilterResponse::Stop) => {
                ctx.blocked = true;
                debug!(stage = "Filter", "消息被过滤规则拦截");
            }
            Ok(FilterResponse::Continue) => {
                trace!(stage = "Filter", "过滤通过");
            }
            Err(e) => {
                warn!(stage = "Filter", error = %e, "消息过滤失败");
            }
        }
        Ok(())
    }
}

/// 框架指令的固定入口；指令启停由运行设置控制，不随插件生命周期变化。
pub struct CoreCommandsRuntimeStage {
    commands: Arc<crate::core_commands::CoreCommands>,
}

impl CoreCommandsRuntimeStage {
    pub fn new(commands: Arc<crate::core_commands::CoreCommands>) -> Self {
        Self { commands }
    }
}

#[async_trait::async_trait]
impl RuntimeEventStageHandler for CoreCommandsRuntimeStage {
    fn stage(&self) -> RuntimeEventStage {
        RuntimeEventStage::CoreCommands
    }

    async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()> {
        self.commands.handle(ctx).await
    }
}

/// Native 插件宿主阶段。
///
/// 保持原有 PluginHost 行为：插件可 Continue、Stop、Reply 或进入 WaitReply；
/// WaitReply 会注册等待会话，并在用户下一次回复时分发回原插件回调。
pub struct PluginHostRuntimeStage {
    plugin_host: Arc<PluginHost>,
    wait_manager: Arc<WaitManager>,
    permission_manager: Arc<PermissionManager>,
}

impl PluginHostRuntimeStage {
    pub fn new(
        plugin_host: Arc<PluginHost>,
        wait_manager: Arc<WaitManager>,
        permission_manager: Arc<PermissionManager>,
    ) -> Self {
        Self {
            plugin_host,
            wait_manager,
            permission_manager,
        }
    }
}

#[async_trait::async_trait]
impl RuntimeEventStageHandler for PluginHostRuntimeStage {
    fn stage(&self) -> RuntimeEventStage {
        RuntimeEventStage::PluginHost
    }

    async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()> {
        let actor_permission = self.permission_manager.evaluate(&ctx.event).await;
        if actor_permission.is_banned() {
            ctx.blocked = true;
            return Ok(());
        }
        let user_id = runtime_identity(&ctx.event, "user_id").ok_or_else(|| {
            crate::error::NuonoError::PermissionValidation(
                "message event has no usable user identity".to_string(),
            )
        })?;
        let group_id = runtime_identity(&ctx.event, "group_id");
        let msg_type = ctx
            .event
            .get("message_type")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();
        let filter_ctx = PluginFilterContext {
            user_id,
            group_id,
            msg_type,
            actor_permission,
        };

        let process_result = self
            .plugin_host
            .process_message_with_context(
                &ctx.event,
                &filter_ctx,
                ctx.domain_event.as_ref(),
                ctx.locale,
            )
            .await;

        self.apply_response(ctx, process_result).await
    }
}

impl PluginHostRuntimeStage {
    /// 回调交互共享普通插件响应和等待会话处理。
    pub(crate) async fn apply_response(
        &self,
        ctx: &mut RuntimeEventContext<'_>,
        process_result: crate::core::plugin_host::PluginProcessResult,
    ) -> Result<()> {
        match process_result.response {
            PluginResponse::Stop => {
                ctx.blocked = true;
                trace!(
                    plugin = process_result.plugin_name.as_deref(),
                    stage = "PluginHost",
                    "插件已接管消息，停止后续分发"
                );
            }
            PluginResponse::Reply(mut reply_pkt) => {
                ctx.blocked = true;
                if let Some(owner) = &process_result.plugin_name {
                    prepare_plugin_response(
                        &self.plugin_host,
                        ctx.bot_id,
                        &ctx.event,
                        owner,
                        &mut reply_pkt,
                        false,
                    )
                    .await?;
                }
                let reply_json = serde_json::to_string(&reply_pkt).unwrap_or_default();
                ctx.send_text(reply_json, "PluginHost").await;
                ctx.blocked = true;
                trace!(
                    plugin = process_result.plugin_name.as_deref(),
                    stage = "PluginHost",
                    "插件返回回复，停止后续分发"
                );
            }
            PluginResponse::WaitReply {
                mut reply_message,
                session_id,
                timeout_secs,
                callback_id,
                ..
            } => {
                ctx.blocked = true;
                let Some(owner) = process_result.plugin_name else {
                    return Ok(());
                };
                let Some(invocation) = process_result.wait_invocation else {
                    return Ok(());
                };
                let Some(tasks) = self.plugin_host.wait_reply_scope(&invocation).await else {
                    return Ok(());
                };
                let mut scope =
                    nuono_plugin::api::waiter::WaitScope::from_event(ctx.bot_id, &ctx.event);
                scope.plugin_name = Some(owner.clone());
                let duration = std::time::Duration::from_secs(timeout_secs as u64);
                // 在发送提问提示前先注册等待会话；若后续提示发送失败，丢弃接收端会自动注销本次等待批次。
                let rx = self.wait_manager.try_register_scoped_event_wait(
                    session_id.clone(),
                    duration,
                    scope,
                )?;
                // 在发送提问提示前先申请受管后台任务槽位；若调用方在准备或发送提示期间被取消，该 oneshot 通道断开也会通知后台任务一并取消。
                let (mut prompt_sent, prompt_ready) = tokio::sync::oneshot::channel();
                let plugin_host = self.plugin_host.clone();
                let task_owner = owner.clone();
                let mut cbid = callback_id;
                let mut sid = session_id.clone();
                let mut duration = duration;
                let mut receiver = rx;
                let bot_id = ctx.bot_id;
                let reply_tx = ctx.bot_sender.clone();
                let connection_bound = ctx.bot_sender.is_some() && ctx.action_sink.is_none();
                let action_sink = ctx.action_sink.clone();
                let action_transport = ctx.action_transport.clone();
                let wait_manager = self.wait_manager.clone();
                let connection_closed = reply_tx.clone().filter(|_| connection_bound);
                if !tasks.try_spawn("wait_reply", async move {
                    let work = async {
                        tokio::select! {
                            biased;
                            _ = receiver.cancelled() => return,
                            ready = prompt_ready => if ready.is_err() { return; },
                        }
                        let owner = task_owner;
                        loop {
                            let Ok(event) = receiver.receive(duration).await else {
                                break;
                            };
                            let response = plugin_host
                                .dispatch_to_plugin_with_context(
                                    &owner,
                                    &event.raw,
                                    &sid,
                                    &cbid,
                                    event.domain.as_ref(),
                                    event.locale,
                                )
                                .await;
                            let (mut reply_pkt, next) = match response {
                                PluginResponse::Reply(reply) => (reply, None),
                                PluginResponse::WaitReply {
                                    reply_message,
                                    session_id,
                                    timeout_secs,
                                    callback_id,
                                    ..
                                } => {
                                    let mut scope =
                                        nuono_plugin::api::waiter::WaitScope::from_event(
                                            bot_id, &event.raw,
                                        );
                                    scope.plugin_name = Some(owner.clone());
                                    let timeout =
                                        std::time::Duration::from_secs(timeout_secs as u64);
                                    let next = match wait_manager.try_register_scoped_event_wait(
                                        session_id.clone(),
                                        timeout,
                                        scope,
                                    ) {
                                        Ok(next) => next,
                                        Err(error) => {
                                            warn!(plugin=%owner,error=%error,"后续等待回复登记失败");
                                            break;
                                        }
                                    };
                                    if connection_bound
                                            && reply_tx
                                                .as_ref()
                                                .is_some_and(|sender| sender.is_closed())
                                    {
                                        break;
                                    }
                                    (
                                        reply_message,
                                        Some((next, session_id, timeout, callback_id)),
                                    )
                                }
                                PluginResponse::Stop | PluginResponse::Continue | _ => break,
                            };
                            if let Some(params) = reply_pkt
                                .get_mut("params")
                                .and_then(|value| value.as_object_mut())
                            {
                                RuntimeEventContext::copy_reply_context_fields(
                                    &event.raw, params, true,
                                );
                                reply_pkt["reply_current"] = serde_json::json!(true);
                            }
                            let publish = async {
                                if let Err(error) = prepare_plugin_response(
                                    &plugin_host,
                                    bot_id,
                                    &event.raw,
                                    &owner,
                                    &mut reply_pkt,
                                    next.is_some(),
                                )
                                .await
                                {
                                    warn!(plugin=%owner,error=%error,"等待回复的富消息准备失败");
                                    return false;
                                }
                                send_runtime_action(
                                    bot_id,
                                    reply_tx.as_ref(),
                                    action_sink.as_ref(),
                                    action_transport.as_ref(),
                                    event.domain.as_ref(),
                                    reply_pkt,
                                    "PluginHostWaitReply",
                                )
                                .await
                            };
                            let sent = if let Some((next, ..)) = &next {
                                tokio::select! {
                                    biased;
                                    _ = next.cancelled() => break,
                                    sent = publish => sent,
                                }
                            } else {
                                publish.await
                            };
                            let Some((next, next_sid, next_duration, next_callback)) = next else {
                                break;
                            };
                            if !sent {
                                break;
                            }
                            receiver = next;
                            sid = next_sid;
                            duration = next_duration;
                            cbid = next_callback;
                        }
                    };
                    // 在整个多轮等待对话期间持续绑定当前插件实例的批次句柄（invocation），确保挂起等待或传输期间插件重载/停用能精确感知。
                    let work = invocation.run(work);
                    if let Some(sender) = connection_closed {
                        tokio::select! { biased; _ = sender.closed() => {}, _ = work => {} }
                    } else {
                        work.await;
                    }
                }) {
                    return Err(crate::error::NuonoError::Overloaded(
                        "插件等待回复后台任务容量已满或插件已停用".into(),
                    ));
                }
                let publish = async {
                    prepare_plugin_response(
                        &self.plugin_host,
                        ctx.bot_id,
                        &ctx.event,
                        &owner,
                        &mut reply_message,
                        true,
                    )
                    .await?;
                    Ok::<_, crate::error::NuonoError>(
                        ctx.send_text(
                            serde_json::to_string(&reply_message).unwrap_or_default(),
                            "PluginHost",
                        )
                        .await,
                    )
                };
                let sent = tokio::select! {
                    biased;
                    _ = prompt_sent.closed() => return Ok(()),
                    sent = publish => sent?,
                };
                if sent {
                    let _ = prompt_sent.send(());
                }
                ctx.blocked = true;
                trace!(session_id = %session_id, stage = "PluginHost", "已登记等待回复，停止后续分发");
            }
            PluginResponse::Continue | _ => {
                trace!(stage = "PluginHost", "继续分发消息");
            }
        }

        Ok(())
    }
}

/// 仅对插件返回的消息正文做按钮签名与交互转换；原始 API 动作与原生目标字段保持不变。
/// 等待会话由外层调用方先完成注册，之后用户点击的自动选项才能被正确路由为回复。
async fn prepare_plugin_response(
    host: &Arc<PluginHost>,
    bot_id: i64,
    event: &serde_json::Value,
    owner: &str,
    packet: &mut serde_json::Value,
    waiting: bool,
) -> Result<()> {
    use nuono_plugin::SendableMessage;
    use nuono_plugin::api::{Bot, MessageContext, button::ButtonAction};
    let Some(message) = packet
        .get_mut("params")
        .and_then(|params| params.get_mut("message"))
    else {
        return Ok(());
    };
    let Some(rich) = nuono_app::rich_message::RichMessage::from_segments(message)? else {
        return Ok(());
    };
    let registered_buttons = rich
        .buttons
        .iter()
        .flatten()
        .filter(|button| button.action != ButtonAction::Jump)
        .collect::<Vec<_>>();
    if !registered_buttons.is_empty()
        && registered_buttons.iter().all(|button| {
            button
                .token
                .as_deref()
                .is_some_and(|token| host.owns_interaction_token(owner, token, event))
        })
    {
        let route = event
            .get("route")
            .cloned()
            .and_then(|route| serde_json::from_value::<crate::core::RuntimeBotRoute>(route).ok())
            .unwrap_or_else(|| {
                crate::core::RuntimeBotRoute::new("onebot", bot_id.to_string(), bot_id.to_string())
            });
        let policy = match host.action_router().interaction_port() {
            Some(port) => port.message_policy(&route).await?,
            None => Default::default(),
        };
        *message = serde_json::json!(
            nuono_app::rich_message::plan(&route.platform, &policy, message)?.segments
        );
        return Ok(());
    }
    let mut prepared_message = nuono_app::rich_message::segments(message)?;
    if waiting {
        for item in &mut prepared_message {
            if item["type"] != "buttons" {
                continue;
            }
            let data = &item["data"];
            let mut rows: Vec<Vec<nuono_plugin::api::button::ButtonOne>> =
                serde_json::from_value(data.get("buttons").unwrap_or(data).clone())?;
            for button in rows.iter_mut().flatten() {
                if button.action == ButtonAction::Auto {
                    button.action = ButtonAction::Reply;
                }
            }
            item["data"]["buttons"] = serde_json::json!(rows);
        }
    }
    let mut current = event.clone();
    current["post_type"] = serde_json::json!("message");
    let mut bot = Bot::new(
        bot_id,
        Arc::new(EventBus::new()),
        host.wait_manager.clone(),
        Arc::new(crate::core::ApiClient::new()),
        None,
        None,
    )
    .with_action_router(host.action_router())
    .with_wait_owner(owner, None);
    if let Some(context) = MessageContext::from_onebot_json(&current) {
        bot.set_current_message(context);
    }
    let (prepared, _) = bot
        .prepare_message(SendableMessage::Json(
            serde_json::json!(prepared_message).to_string(),
        ))
        .await?;
    *message = prepared;
    Ok(())
}

/// 触发器分发阶段。
///
/// 保持原有 `EventHandler` 行为：匹配成功后记录日志，并按 handler 返回值
/// 决定是否阻断后续 Matcher / AI / Publish 阶段。
pub struct TriggerDispatchRuntimeStage {
    event_handler: Arc<EventHandler>,
}

impl TriggerDispatchRuntimeStage {
    pub fn new(_db: Database, event_handler: Arc<EventHandler>) -> Self {
        Self { event_handler }
    }
}

#[async_trait::async_trait]
impl RuntimeEventStageHandler for TriggerDispatchRuntimeStage {
    fn stage(&self) -> RuntimeEventStage {
        RuntimeEventStage::TriggerDispatch
    }

    async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()> {
        trace!(
            bot_id = ctx.bot_id,
            stage = "EventHandler",
            "开始匹配触发器"
        );
        // `ctx.event` 已在 runtime 入口注入跨平台 route；必须用它而非原始 wire 文本，
        // 否则 Command 看不到 Event::route()，无法安全保存后续主动推送目标。
        let event_json = ctx.event.to_string();
        match self
            .event_handler
            .handle_event_with_context(
                ctx.bot_id,
                &event_json,
                ctx.bot_sender.clone(),
                ctx.action_sink.clone(),
                ctx.domain_event.as_ref(),
                ctx.locale,
            )
            .await
        {
            Ok(result) => {
                trace!(
                    bot_id = %ctx.bot_id,
                    matched = result.matched,
                    propagation_stopped = result.blocked,
                    "触发器分发结束"
                );
                ctx.blocked = result.blocked;
            }
            Err(e) => {
                warn!(bot_id = %ctx.bot_id, error = %e, "EventHandler 执行失败");
            }
        }
        Ok(())
    }
}

/// Matcher 分发阶段。
///
/// 保持原有 NoneBot2 风格 matcher 行为：只处理可解析的消息事件；
/// matcher 产生回复时发送所有回复并阻断，未回复时沿用 matcher 的 blocked。
pub struct MatcherRuntimeStage {
    matcher_registry: Arc<MatcherRegistry>,
    adapter: Arc<dyn PlatformAdapter>,
}

impl MatcherRuntimeStage {
    pub fn new(matcher_registry: Arc<MatcherRegistry>, adapter: Arc<dyn PlatformAdapter>) -> Self {
        Self {
            matcher_registry,
            adapter,
        }
    }
}

#[async_trait::async_trait]
impl RuntimeEventStageHandler for MatcherRuntimeStage {
    fn stage(&self) -> RuntimeEventStage {
        RuntimeEventStage::Matcher
    }

    async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()> {
        let unified_event = if let Some(unified_event) = &ctx.unified_event {
            unified_event.clone()
        } else {
            let Ok(unified_event) = self.adapter.parse_incoming(ctx.raw_text) else {
                return Ok(());
            };
            unified_event
        };
        let UnifiedEvent::Message(_) = unified_event else {
            return Ok(());
        };

        let result = self
            .matcher_registry
            .handle_event(unified_event.clone())
            .await;
        if !result.replies.is_empty() {
            for reply in &result.replies {
                ctx.send_text(reply.clone(), "Matcher").await;
            }
            debug!(bot_id = %ctx.bot_id, reply_count = result.replies.len(), "✅ Matcher 匹配成功");
            ctx.blocked = true;
        } else {
            ctx.blocked = result.blocked;
        }

        Ok(())
    }
}

/// Agent Runtime 阶段。
///
/// 保持原有触发策略：私聊默认触发；群聊需要 @Bot 或 `/ai ` 前缀。
/// 命中后提交 Agent 任务并释放入站管线；后台任务保留原生回复 transport。
pub struct AgentRuntimeStage {
    agent_runtime: AgentRuntime,
    db: Database,
    config: Arc<crate::config::ConfigManager>,
}

impl AgentRuntimeStage {
    pub fn with_runtime(
        agent_runtime: AgentRuntime,
        db: Database,
        config: Arc<crate::config::ConfigManager>,
    ) -> Self {
        Self {
            agent_runtime,
            db,
            config,
        }
    }
    pub fn new(
        db: Database,
        config: Arc<crate::config::ConfigManager>,
        tool_registry: Arc<ToolRegistry>,
    ) -> Self {
        let agent_runtime =
            AgentRuntime::with_tool_registry(db.clone(), config.clone(), tool_registry);
        Self::with_runtime(agent_runtime, db, config)
    }
}

/// 从入站事件上下文中提取发言人的平台身份，用于映射所属站点账号的 AI 准入状态。
fn sender_identity_from_context(
    ctx: &RuntimeEventContext<'_>,
) -> Option<nuono_domain::IdentityRef> {
    if let Some(event) = &ctx.domain_event {
        return event
            .actor
            .clone()
            .or_else(|| event.message().map(|message| message.sender.clone()));
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
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
            })
    })?;
    nuono_domain::IdentityRef::new(
        nuono_domain::Platform::OneBot,
        nuono_domain::IdentityKind::User,
        nuono_domain::IdentityScheme::Native,
        user_id,
        None::<String>,
    )
    .ok()
}

/// 从入站事件上下文解析内部群对象 ID（`nuono_groups.id`），用于判断该群是否被暂停 AI 自动回复。
async fn group_id_from_context(db: &Database, ctx: &RuntimeEventContext<'_>) -> Option<i64> {
    if let Some(event) = &ctx.domain_event {
        let conversation = event.conversation.as_ref()?;
        return db
            .group_for_conversation(conversation)
            .await
            .ok()
            .flatten()
            .map(|group| group.id);
    }
    if ctx.event.get("message_type").and_then(|v| v.as_str()) != Some("group") {
        return None;
    }
    let native_group_id = ctx.event.get("group_id").and_then(|value| {
        value
            .as_i64()
            .filter(|id| *id > 0)
            .map(|id| id.to_string())
            .or_else(|| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
            })
    })?;
    db.group_for_onebot(&native_group_id)
        .await
        .ok()
        .flatten()
        .map(|group| group.id)
}

#[async_trait::async_trait]
impl RuntimeEventStageHandler for AgentRuntimeStage {
    fn stage(&self) -> RuntimeEventStage {
        RuntimeEventStage::AgentRuntime
    }

    async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()> {
        let config_snapshot = self.config.snapshot().await;
        let ai_cfg = &config_snapshot.ai;
        if !ai_cfg.blocked_accounts.is_empty()
            && let Some(identity) = sender_identity_from_context(ctx)
            && let Ok(Some(account_id)) = self.db.account_for_platform_identity(&identity).await
            && ai_cfg.is_account_blocked(account_id)
        {
            return Ok(());
        }
        if !ai_cfg.blocked_groups.is_empty()
            && let Some(group_id) = group_id_from_context(&self.db, ctx).await
            && ai_cfg.is_group_blocked(group_id)
        {
            return Ok(());
        }
        let context = if let Some(event) = &ctx.domain_event {
            nuono_app::agent::AgentContext::from_domain_event(event)
        } else {
            nuono_app::agent::AgentContext::from_runtime_event(ctx.bot_id, &ctx.event)
        };
        let Some(context) = context else {
            return Ok(());
        };
        let policy = self.agent_runtime.output_policy(&context).await;
        let observe = policy.mode == nuono_app::agent::output::OutputMode::Segmented
            || (policy.mode == nuono_app::agent::output::OutputMode::Native
                && crate::agent::output::supports_native(&context));
        let (progress, receiver) = tokio::sync::watch::channel(Default::default());
        let submitted = if observe {
            self.agent_runtime
                .submit_text_with_output(context, progress)
                .await
        } else {
            drop(progress);
            self.agent_runtime.submit_text(context).await
        };
        let task = match submitted {
            Ok(Some(task)) => task,
            Ok(None) => return Ok(()),
            Err(error) => {
                ctx.send_reply_text(format!("Agent 任务失败：{error}"), "AgentRuntime")
                    .await;
                ctx.blocked = true;
                return Ok(());
            }
        };
        // 提取并保留原生传输通道或旧版发送端供后台回复任务使用，同时立即释放入站事件循环。
        let reply_context = ctx.reply_context();
        let delivery = self
            .agent_runtime
            .spawn_reply(async move {
                crate::agent::output::deliver(reply_context, task, receiver, policy).await;
            })
            .await;
        if let Err(error) = delivery {
            ctx.send_reply_text(format!("Agent 任务失败：{error}"), "AgentRuntime")
                .await;
        }
        ctx.blocked = true;
        Ok(())
    }
}

/// 事件发布阶段。
///
/// 保存入站消息历史，并向事件总线发布 `MessageReceived`。
pub struct EventPublishRuntimeStage {
    db: Database,
    event_bus: Arc<EventBus>,
}

impl EventPublishRuntimeStage {
    pub fn new(db: Database, event_bus: Arc<EventBus>) -> Self {
        Self { db, event_bus }
    }
}

#[async_trait::async_trait]
impl RuntimeEventStageHandler for EventPublishRuntimeStage {
    fn stage(&self) -> RuntimeEventStage {
        RuntimeEventStage::EventPublish
    }

    async fn handle(&self, ctx: &mut RuntimeEventContext<'_>) -> Result<()> {
        let bid = ctx
            .domain_event
            .as_ref()
            .map(|event| event.bot.account_id.value().to_string())
            .unwrap_or_else(|| ctx.bot_id.to_string());
        let mut history_event = ctx.event.clone();
        if let Some(domain_event) = ctx.domain_event.as_ref()
            && let Some(obj) = history_event.as_object_mut()
        {
            obj.entry("platform".to_owned())
                .or_insert_with(|| serde_json::json!(domain_event.bot.platform().as_str()));
            let instance_id = domain_event.bot.instance_id.as_str();
            if !instance_id.is_empty() {
                obj.entry("bot_instance_id".to_owned())
                    .or_insert_with(|| serde_json::json!(instance_id));
            }
        }
        // 采用同步有界通道尽力入队（try_send），队列满时的丢弃计数由写入端统计；
        // 绝不能在这里 spawn 异步任务等待入队，否则通道满时会演变成无上限的后台任务积压。
        let _ = self
            .db
            .save_message_history(&bid, &history_event, "inbound")
            .await;
        self.event_bus
            .publish(Event::new(
                EventType::MessageReceived,
                ctx.bot_id,
                ctx.event.clone(),
            ))
            .await;

        Ok(())
    }
}
