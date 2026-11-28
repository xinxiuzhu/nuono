use std::sync::Arc;

mod memory;
mod retention;

#[cfg(test)]
#[path = "../../tests/unit/service/assistant.rs"]
mod tests;

use dashmap::DashSet;
use nuono_app::{
    agent::{AgentContext, companion_scope_key, input::HistoryMessage},
    assistant::{
        AssistantAdmission, AssistantTurn, MAX_MESSAGE_BYTES, TurnDelivery, TurnFailure,
        TurnStatus, valid_qq,
    },
};
use nuono_domain::{ConversationRef, MessageSegment, Platform};
use sha2::{Digest, Sha256};

use crate::{
    agent::AgentRuntime,
    config::ConfigManager,
    core::event_kernel::RuntimeEventContext,
    db::{AssistantTurnAdmission, AssistantTurnRequest, Database},
    error::{NuonoError, Result},
};

#[derive(Clone)]
pub struct Assistant {
    db: Database,
    config: Arc<ConfigManager>,
    runtime: AgentRuntime,
    busy: Arc<DashSet<i64>>,
    deleting: Arc<DashSet<i64>>,
    maintenance: Arc<DashSet<i64>>,
    foreground_waiting: Arc<DashSet<i64>>,
    privacy_blocked: Arc<DashSet<i64>>,
    journal: super::privacy_journal::PrivacyJournal,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct AssistantView {
    pub active_card: nuono_app::companion::CompanionCard,
    pub enabled: bool,
    pub available: bool,
    pub allow_user_custom_cards: bool,
    pub has_platform_binding: bool,
    pub remaining: u32,
    pub daily_limit: u32,
    #[serde(with = "nuono_domain::scalar::optional_utc_millis")]
    #[schema(value_type = Option<String>)]
    #[serde(default)]
    pub grant_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    #[schema(value_type = String)]
    pub quota_resets_at: chrono::DateTime<chrono::Utc>,
    pub access_state: &'static str,
    pub busy: bool,
    pub qq_bot: Option<String>,
    pub qq: Option<String>,
    pub messages: Vec<AssistantMessage>,
    pub turns: Vec<AssistantTurn>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct AssistantMessage {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub role: String,
    pub content: String,
}

struct Turn {
    busy: Arc<DashSet<i64>>,
    account_id: i64,
}

/// 同时持有源账号与目标账号的助手准入锁与隐私删除屏障；
/// 无论是正常完成、异步取消还是事务回滚，析构时都会自动释放这两道屏障。
pub struct AccountMergeGuard {
    _guards: Vec<Turn>,
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.busy.remove(&self.account_id);
    }
}

/// 跨端私聊投递来源：区分管理员指定的专用 QQ 机器人与普通已绑定账号的跨端私聊同步。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryRoute {
    DedicatedQq,
    SyncedDirect,
}

struct PlatformDelivery {
    identity: nuono_domain::IdentityRef,
    route: DeliveryRoute,
    reply: RuntimeEventContext<'static>,
}

enum AssistantDelivery {
    Platform(PlatformDelivery),
    WebChat {
        reply: RuntimeEventContext<'static>,
        done: tokio::sync::oneshot::Sender<()>,
    },
}

/// 将全局 AI 总开关叠加到个人对话配置上；总开关关闭时网页对话与跨端私聊一并停用。
fn effective_assistant_settings(
    ai: &crate::config::AIConfig,
) -> nuono_app::assistant::AssistantSettings {
    let mut settings = ai.assistant.clone();
    settings.enabled = ai.assistant_active();
    settings
}

/// 判断已绑定的平台身份是否属于当前支持跨端私聊同步的平台与私聊范围。
fn is_direct_sync_identity(platform: &str, namespace: &str) -> bool {
    matches!(platform, "onebot" | "qq_official") && !namespace.contains("/groups/")
}

/// 判断入站私聊事件是否满足 OneBot 好友私聊或 QQ 官方 C2C 私聊的同步条件。
fn supports_direct_sync(
    event: &nuono_domain::DomainEvent,
    message: &nuono_domain::DomainMessage,
) -> bool {
    match event.bot.platform() {
        Platform::OneBot => {
            message
                .raw
                .get("sub_type")
                .and_then(serde_json::Value::as_str)
                == Some("friend")
                && valid_qq(message.sender.value().as_str())
        }
        Platform::QqOfficial => !message.sender.value().as_str().trim().is_empty(),
        _ => false,
    }
}

impl Assistant {
    pub(crate) fn new(
        db: Database,
        config: Arc<ConfigManager>,
        runtime: AgentRuntime,
        paths: &crate::core::Paths,
    ) -> Self {
        Self {
            db,
            config,
            runtime,
            busy: Arc::new(DashSet::new()),
            deleting: Arc::new(DashSet::new()),
            maintenance: Arc::new(DashSet::new()),
            foreground_waiting: Arc::new(DashSet::new()),
            privacy_blocked: Arc::new(DashSet::new()),
            journal: super::privacy_journal::PrivacyJournal::new(&paths.data),
        }
    }

    pub fn guard_account_merge(&self, source: i64, target: i64) -> Result<AccountMergeGuard> {
        let mut ids = vec![source, target];
        ids.sort_unstable();
        ids.dedup();
        let mut guards = Vec::new();
        for account_id in ids {
            self.ensure_privacy_ready(account_id)?;
            if !self.deleting.insert(account_id) {
                return Err(NuonoError::Conflict(
                    "个人数据清理或账号合并正在进行，请稍后重试".into(),
                ));
            }
            guards.push(Turn {
                busy: self.deleting.clone(),
                account_id,
            });
            if !self.busy.insert(account_id) {
                return Err(NuonoError::Conflict(
                    "请等待两个账号的私人任务和清理完成后再合并".into(),
                ));
            }
            guards.push(Turn {
                busy: self.busy.clone(),
                account_id,
            });
            self.ensure_privacy_ready(account_id)?;
        }
        Ok(AccountMergeGuard { _guards: guards })
    }

    pub async fn view(&self, account_id: i64) -> Result<AssistantView> {
        if !self.active_account(account_id).await? {
            return Err(NuonoError::PermissionDenied("请使用有效账号登录".into()));
        }
        self.wait_for_privacy_ready(account_id).await?;
        self.reconcile_idle(account_id).await?;
        let config = self.config.snapshot().await;
        let blocked = config.ai.is_account_blocked(account_id);
        let settings = effective_assistant_settings(&config.ai);
        let account = self.db.assistant_account(account_id, &settings).await?;
        let busy = self.busy.contains(&account_id);
        let card = self.db.ensure_default_companion_card(account_id).await?;
        let messages = self
            .db
            .companion_conversations(account_id, &card.id, 80)
            .await?
            .into_iter()
            .filter_map(|row| {
                let (role, content) = if matches!(row.role.as_str(), "assistant" | "user") {
                    (row.role, row.content)
                } else {
                    (
                        "user".into(),
                        HistoryMessage::decode(&row.role, &row.content)?.content,
                    )
                };
                Some(AssistantMessage {
                    id: row.id,
                    role,
                    content,
                })
            })
            .collect();
        let turns = self
            .db
            .assistant_turns_for_card(account_id, &card.id)
            .await?;
        let has_platform_binding = self
            .db
            .platform_identities(account_id)
            .await?
            .iter()
            .any(|id| is_direct_sync_identity(&id.platform, &id.namespace));
        Ok(AssistantView {
            active_card: card,
            enabled: settings.enabled,
            available: account.available && !blocked,
            allow_user_custom_cards: settings.allow_user_custom_cards,
            has_platform_binding,
            remaining: account.remaining,
            daily_limit: account.daily_limit,
            grant_expires_at: account.grant_expires_at,
            quota_resets_at: account.quota_resets_at,
            access_state: if !settings.enabled {
                "disabled"
            } else if blocked {
                "blocked"
            } else if !nuono_app::plugin_extensions::assistant_available() {
                "service_unavailable"
            } else if !account.available
                && account
                    .grant_expires_at
                    .is_some_and(|until| until <= chrono::Utc::now())
            {
                "expired"
            } else if !account.available {
                "authorization_required"
            } else if account.remaining == 0 {
                "exhausted"
            } else {
                "ready"
            },
            busy,
            qq_bot: settings.qq_bot_id.clone(),
            qq: account.qq,
            messages,
            turns,
        })
    }

    async fn active_account(&self, account_id: i64) -> Result<bool> {
        Ok(self
            .db
            .find_user_by_id(account_id)
            .await?
            .is_some_and(|user| user.status == crate::db::STATUS_ACTIVE))
    }

    pub async fn send_web(
        &self,
        account_id: i64,
        request_id: uuid::Uuid,
        text: &str,
    ) -> Result<AssistantAdmission> {
        self.submit(
            account_id,
            format!("web:{}", request_id.simple()),
            Some(text.to_owned()),
            None,
            None,
        )
        .await
    }

    pub async fn retry_web(
        &self,
        account_id: i64,
        request_id: uuid::Uuid,
        turn_id: String,
    ) -> Result<AssistantAdmission> {
        self.submit(
            account_id,
            format!("web:{}", request_id.simple()),
            None,
            Some(turn_id),
            None,
        )
        .await
    }

    async fn reconcile_idle(&self, account_id: i64) -> Result<()> {
        if self.busy.insert(account_id) {
            let _turn = Turn {
                busy: self.busy.clone(),
                account_id,
            };
            self.db.reconcile_assistant_account(account_id).await?;
        }
        Ok(())
    }

    async fn submit(
        &self,
        account_id: i64,
        request_id: String,
        text: Option<String>,
        retry_of: Option<String>,
        delivery: Option<AssistantDelivery>,
    ) -> Result<AssistantAdmission> {
        if text
            .as_ref()
            .is_some_and(|text| text.trim().is_empty() || text.trim().len() > MAX_MESSAGE_BYTES)
        {
            return Err(NuonoError::Validation(
                "请发送 1 至 4096 字节的文字消息".into(),
            ));
        }
        self.ensure_privacy_ready(account_id)?;
        if self.deleting.contains(&account_id) {
            return Err(NuonoError::Conflict(
                "正在清理个人聊天数据，请稍后再发送".into(),
            ));
        }
        if !self.active_account(account_id).await? {
            return Ok(AssistantAdmission::Unavailable);
        }
        if self
            .config
            .snapshot()
            .await
            .ai
            .is_account_blocked(account_id)
        {
            return Err(NuonoError::PermissionDenied(
                "管理员已暂停你的 AI 使用权限".into(),
            ));
        }
        if self
            .db
            .assistant_request_replayed(
                account_id,
                &request_id,
                text.as_deref(),
                retry_of.as_deref(),
            )
            .await?
        {
            return Ok(AssistantAdmission::Duplicate);
        }
        let _waiting = self.interrupt_maintenance(account_id).await?;
        if !self.busy.insert(account_id) {
            return Err(NuonoError::Conflict(
                "助手正在回复，请等当前消息完成".into(),
            ));
        }
        let turn = Turn {
            busy: self.busy.clone(),
            account_id,
        };
        if self.deleting.contains(&account_id) {
            return Err(NuonoError::Conflict(
                "正在清理个人聊天数据，请稍后再发送".into(),
            ));
        }
        self.ensure_privacy_ready(account_id)?;
        let service = self.clone();
        let (admitted, acknowledgement) = tokio::sync::oneshot::channel();
        self.runtime
            .spawn_settling_reply(move |mut stop| async move {
                let _turn = turn;
                if *stop.borrow() {
                    let _ = admitted.send(Err(NuonoError::Conflict("助手正在重启，请稍后重试".into())));
                    return;
                }
                let preparation = async {
                    service.db.reconcile_assistant_account(account_id).await?;
                    let config = service.config.snapshot().await;
                    let effective = effective_assistant_settings(&config.ai);
                    service.db.start_assistant_turn(account_id, &effective, AssistantTurnRequest {
                        request_key: &request_id, text: text.as_deref(), retry_of: retry_of.as_deref(),
                        sender: match delivery.as_ref() {
                            Some(AssistantDelivery::Platform(platform)) => Some(&platform.identity),
                            Some(AssistantDelivery::WebChat { .. }) | None => None,
                        },
                    }).await
                }.await;
                let receipt = match preparation {
                    Ok(AssistantTurnAdmission::Accepted(receipt)) => receipt,
                    Ok(other) => {
                        let result = match other {
                            AssistantTurnAdmission::Duplicate => AssistantAdmission::Duplicate,
                            AssistantTurnAdmission::Unavailable => AssistantAdmission::Unavailable,
                            AssistantTurnAdmission::Exhausted => AssistantAdmission::Exhausted,
                            AssistantTurnAdmission::Accepted(_) => unreachable!("accepted handled above"),
                        };
                        let _ = admitted.send(Ok(result));
                        return;
                    }
                    Err(error) => { let _ = admitted.send(Err(error)); return; }
                };
                let _ = admitted.send(Ok(AssistantAdmission::Accepted));
                let fallback = if *stop.borrow() { TurnFailure::Interrupted } else {
                    let work = async {
                        if receipt.control {
                            service.process_memory_control(account_id, &receipt).await?;
                            Ok::<_, NuonoError>(Some(()))
                        } else {
                            let personal = service.db.personal_profile(account_id).await?;
                            let context = AgentContext::for_companion_turn(account_id, &receipt).with_personal_profile(&personal);
                            let task = service.runtime.submit_text(context).await?;
                            match task { Some(task) => task.await.map(|_| Some(())), None => Ok(None) }
                        }
                    };
                    tokio::select! {
                        biased;
                        _ = stop.changed() => TurnFailure::Interrupted,
                        result = work => match result {
                            Ok(Some(_)) => TurnFailure::ModelFailed,
                            Ok(None) => TurnFailure::Unavailable,
                            Err(error) => { tracing::warn!(error_kind = error.kind(), "助手执行失败"); TurnFailure::ModelFailed }
                        }
                    }
                };
                if *stop.borrow()
                    && let Err(error) = service.runtime.wait_for_idle().await
                {
                    tracing::warn!(error_kind = error.kind(), "助手中断后等待执行收尾失败，将按持久化结果核对");
                }
                let fallback = if *stop.borrow() { TurnFailure::Interrupted } else { fallback };
                let settled = service.settle(account_id, &receipt.id, fallback).await;
                match delivery {
                    Some(AssistantDelivery::Platform(platform)) => {
                        if let Some(receipt) = settled.as_ref()
                            && !*stop.borrow()
                        {
                            tokio::select! {
                                biased;
                                _ = stop.changed() => {},
                                () = service.deliver_receipt(account_id, receipt, &request_id, platform) => {},
                            }
                        }
                    }
                    Some(AssistantDelivery::WebChat { reply, done }) => {
                        if let Some(receipt) = settled.as_ref()
                            && !*stop.borrow()
                        {
                            let response = receipt
                                .failure
                                .map(|failure| failure.message().to_owned())
                                .or_else(|| receipt.reply.clone());
                            if let Some(response) = response {
                                tokio::select! {
                                    biased;
                                    _ = stop.changed() => {},
                                    _ = reply.send_reply_text(response, "Assistant") => {},
                                }
                            }
                        }
                        let _ = done.send(());
                    }
                    None => {}
                }
                if let Some(saved) = settled.as_ref().filter(|turn| turn.status == TurnStatus::Succeeded && !turn.control)
                    && !*stop.borrow() && !service.deleting.contains(&account_id) {
                    service.maintenance.insert(account_id);
                    let _maintenance = Turn { busy: service.maintenance.clone(), account_id };
                    let capture = service.capture_successful_experience(account_id, saved);
                    tokio::pin!(capture);
                    let mut interruption = tokio::time::interval(std::time::Duration::from_millis(25));
                    loop {
                        tokio::select! {
                            biased;
                            _ = stop.changed() => break,
                            _ = interruption.tick() => {
                                if service.deleting.contains(&account_id) || service.foreground_waiting.contains(&account_id) { break; }
                            }
                            () = &mut capture => break,
                        }
                    }
                }
            })
            .await?;
        acknowledgement
            .await
            .map_err(|_| NuonoError::Conflict("助手已停止接收消息".into()))?
    }

    async fn settle(
        &self,
        account_id: i64,
        id: &str,
        fallback: TurnFailure,
    ) -> Option<AssistantTurn> {
        // 结算暂时失败时保留 running；后续读取和启动恢复会重新核对，不谎报退款。
        for delay in [0, 100, 300] {
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            match self
                .db
                .settle_assistant_turn(account_id, id, fallback)
                .await
            {
                Ok(receipt) => return receipt,
                Err(error) => {
                    tracing::error!(error_kind = error.kind(), turn_id = id, "助手结算暂未完成")
                }
            }
        }
        None
    }

    async fn deliver_receipt(
        &self,
        account_id: i64,
        receipt: &AssistantTurn,
        key: &str,
        delivery: PlatformDelivery,
    ) {
        if self
            .db
            .begin_assistant_delivery(account_id, &receipt.id, key)
            .await
            .ok()
            != Some(true)
        {
            return;
        }
        let response = receipt
            .failure
            .map(|failure| failure.message().to_owned())
            .or_else(|| receipt.reply.clone());
        if let Some(response) = response
            && self.deliver_platform(account_id, delivery, response).await
            && let Err(error) = self
                .db
                .confirm_assistant_delivery(account_id, &receipt.id, key)
                .await
        {
            tracing::warn!(error_kind = error.kind(), "助手平台投递回执保存失败");
        }
    }

    pub async fn begin_qq_link(
        &self,
        account_id: i64,
        qq: &str,
        session: &str,
    ) -> Result<(String, crate::db::PlatformBindingIssued)> {
        let config = self.config.snapshot().await;
        let (bot, instance) = config
            .ai
            .assistant
            .qq_route()
            .ok_or_else(|| NuonoError::Conflict("当前未配置助手的 QQ 机器人".into()))?;
        let qq = qq.trim();
        if !valid_qq(qq) || qq == bot {
            return Err(NuonoError::Validation("请输入你自己的 QQ 号".into()));
        }
        let bot_identity = nuono_domain::BotIdentity::new(
            instance,
            nuono_domain::IdentityRef::new(
                Platform::OneBot,
                nuono_domain::IdentityKind::Bot,
                nuono_domain::IdentityScheme::Native,
                bot,
                None::<String>,
            )
            .map_err(|_| NuonoError::Validation("机器人身份无效".into()))?,
            None,
        )
        .map_err(|_| NuonoError::Validation("机器人身份无效".into()))?;
        let sender = nuono_domain::IdentityRef::new(
            Platform::OneBot,
            nuono_domain::IdentityKind::User,
            nuono_domain::IdentityScheme::Native,
            qq,
            None::<String>,
        )
        .map_err(|_| NuonoError::Validation("用户身份无效".into()))?;
        let issued = self
            .db
            .begin_platform_binding(account_id, bot_identity, session, Some(sender))
            .await?;
        Ok((bot.to_owned(), issued))
    }

    pub(crate) async fn handle_binding_event(&self, ctx: &RuntimeEventContext<'_>) -> bool {
        let Some(event) = &ctx.domain_event else {
            return false;
        };
        let Some(message) = event.message() else {
            return false;
        };
        if event.actor.as_ref() != Some(&message.sender) || event.conversation.is_none() {
            return false;
        }
        let text: String = message
            .segments
            .iter()
            .filter_map(|segment| match segment {
                MessageSegment::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let Some(code) = crate::core_commands::binding_code(&text) else {
            return false;
        };
        if !(-60..=300).contains(&(chrono::Utc::now() - event.occurred_at).num_seconds()) {
            return true;
        }
        let reply = match self
            .db
            .observe_platform_binding(code, &event.bot, &message.sender)
            .await
        {
            Ok(true) => "已收到身份验证，请回到发起绑定的控制台核对并确认。",
            Ok(false) => "绑定请求无效、已被领取或平台范围不匹配，请回控制台查看。",
            Err(error) => {
                tracing::warn!(error_kind = error.kind(), "平台绑定验证失败");
                "绑定暂时不可用，请稍后重试。"
            }
        };
        ctx.send_reply_text(reply.into(), "AccountBinding").await;
        true
    }

    pub async fn unlink_qq(&self, account_id: i64) -> Result<()> {
        self.db.unlink_assistant_qq(account_id).await
    }

    async fn deliver_platform(
        &self,
        account_id: i64,
        delivery: PlatformDelivery,
        response: String,
    ) -> bool {
        let config = self.config.snapshot().await;
        if !config.ai.enabled || config.ai.is_account_blocked(account_id) {
            return false;
        }
        let settings = &config.ai.assistant;
        match delivery.route {
            DeliveryRoute::DedicatedQq => {
                let Some((bot, instance)) = settings.qq_route() else {
                    return false;
                };
                if !delivery.reply.domain_event.as_ref().is_some_and(|event| {
                    event.bot.account_id.value().as_str() == bot
                        && event.bot.instance_id.as_str() == instance
                }) {
                    return false;
                }
            }
            DeliveryRoute::SyncedDirect => {
                if !settings.enabled {
                    return false;
                }
            }
        }
        if self.active_account(account_id).await.ok() != Some(true) {
            return false;
        }
        if self
            .db
            .account_for_platform_identity(&delivery.identity)
            .await
            .ok()
            .flatten()
            == Some(account_id)
        {
            return delivery.reply.send_reply_text(response, "Assistant").await;
        }
        false
    }

    pub(crate) async fn handle_event(&self, ctx: &RuntimeEventContext<'_>) -> bool {
        let Some(event) = &ctx.domain_event else {
            return false;
        };
        let Some(message) = event.message() else {
            return false;
        };
        let Some(ConversationRef::Direct { participant, .. }) = &event.conversation else {
            return false;
        };
        if participant != &message.sender
            || event.actor.as_ref() != Some(&message.sender)
            || message.sender.value() == event.bot.account_id.value()
            || !supports_direct_sync(event, message)
        {
            return false;
        }
        let config = self.config.snapshot().await;
        if !config.ai.enabled {
            return false;
        }
        let settings = &config.ai.assistant;
        let route = if event.bot.platform() == &Platform::OneBot
            && settings.qq_route().is_some_and(|(bot, instance)| {
                event.bot.account_id.value().as_str() == bot
                    && event.bot.instance_id.as_str() == instance
            }) {
            DeliveryRoute::DedicatedQq
        } else {
            DeliveryRoute::SyncedDirect
        };
        if route == DeliveryRoute::SyncedDirect && !settings.enabled {
            return false;
        }
        let account_id = match self.db.account_for_platform_identity(&message.sender).await {
            Ok(Some(account_id)) => account_id,
            Ok(None) => {
                if route == DeliveryRoute::DedicatedQq {
                    let age = (chrono::Utc::now() - event.occurred_at).num_seconds();
                    if (-60..=300).contains(&age) && message.message_id.as_str().len() <= 64 {
                        ctx.send_reply_text(
                            "请先在 Nuono 控制台验证并绑定 QQ。".into(),
                            "Assistant",
                        )
                        .await;
                    }
                    return true;
                }
                return false;
            }
            Err(error) => {
                tracing::warn!(error_kind = error.kind(), "助手绑定读取失败");
                if route == DeliveryRoute::DedicatedQq {
                    ctx.send_reply_text("助手暂时无法回复，请稍后重试。".into(), "Assistant")
                        .await;
                    return true;
                }
                return false;
            }
        };
        if config.ai.is_account_blocked(account_id) {
            return false;
        }
        if route == DeliveryRoute::SyncedDirect
            && !self
                .db
                .assistant_account(account_id, settings)
                .await
                .is_ok_and(|account| account.available)
        {
            return false;
        }
        let age = (chrono::Utc::now() - event.occurred_at).num_seconds();
        if !(-60..=300).contains(&age) || message.message_id.as_str().len() > 64 {
            return true;
        }
        let text: String = message
            .segments
            .iter()
            .filter_map(|segment| match segment {
                MessageSegment::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let reply = self
            .platform_message(
                ctx,
                &event.bot,
                account_id,
                message.sender.clone(),
                route,
                message.message_id.as_str(),
                &text,
            )
            .await;
        if let Some(reply) = reply {
            ctx.send_reply_text(reply.into(), "Assistant").await;
        }
        true
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "内部私聊派发函数集中接收已解包的事件上下文与身份字段，无需为此额外包装一次性参数结构体"
    )]
    async fn platform_message(
        &self,
        ctx: &RuntimeEventContext<'_>,
        bot: &nuono_domain::BotIdentity,
        account_id: i64,
        sender: nuono_domain::IdentityRef,
        route: DeliveryRoute,
        message_id: &str,
        text: &str,
    ) -> Option<&'static str> {
        let identity = match serde_json::to_vec(bot) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, "跨平台机器人身份序列化失败");
                return Some("暂时无法回复，请稍后重试。");
            }
        };
        let key = format!("qq:{}:{message_id}", hex::encode(Sha256::digest(identity)));
        if self.db.assistant_request_seen(account_id, &key).await.ok() == Some(true) {
            return None;
        }
        if self.reconcile_idle(account_id).await.is_err() {
            return Some("上次对话结果正在核对，请稍后重试。");
        }
        let latest = self
            .db
            .assistant_turns(account_id)
            .await
            .ok()
            .and_then(|turns| turns.into_iter().last());
        let retry_of = if text.trim() == "重试" {
            match latest.as_ref() {
                Some(receipt) if receipt.retryable => Some(receipt.id.clone()),
                Some(receipt) if receipt.status == TurnStatus::Succeeded => {
                    let delivery = PlatformDelivery {
                        identity: sender,
                        route,
                        reply: ctx.reply_context(),
                    };
                    return self
                        .redeliver_success(account_id, &receipt.id, &key, delivery)
                        .await;
                }
                Some(receipt) if receipt.status == TurnStatus::Running => {
                    return Some("助手正在回复，请等当前消息完成。");
                }
                _ => return Some("暂无可重试的消息，请直接发送新的聊天内容。"),
            }
        } else {
            if let Some(receipt) = latest.as_ref().filter(|receipt| {
                receipt.status == TurnStatus::Interrupted
                    && receipt.delivery == TurnDelivery::Pending
            }) {
                self.deliver_receipt(
                    account_id,
                    receipt,
                    &format!("notice:{}", receipt.id),
                    PlatformDelivery {
                        identity: sender.clone(),
                        route,
                        reply: ctx.reply_context(),
                    },
                )
                .await;
            }
            None
        };
        let delivery = PlatformDelivery {
            identity: sender,
            route,
            reply: ctx.reply_context(),
        };
        match self
            .submit(
                account_id,
                key,
                retry_of.is_none().then(|| text.to_owned()),
                retry_of,
                Some(AssistantDelivery::Platform(delivery)),
            )
            .await
        {
            Ok(AssistantAdmission::Accepted | AssistantAdmission::Duplicate) => None,
            Ok(AssistantAdmission::Unavailable) => Some("对话服务当前不可用。"),
            Ok(AssistantAdmission::Exhausted) => Some("今天的对话额度已用完。"),
            Err(NuonoError::Conflict(_)) => Some("正在回复上一条消息，请稍候。"),
            Err(NuonoError::Validation(_)) => Some("请发送 1 至 4096 字节的文字消息。"),
            Err(error) => {
                tracing::warn!(error_kind = error.kind(), "跨平台对话请求失败");
                Some("暂时无法回复，请稍后重试。")
            }
        }
    }

    /// 处理网页对话中未命中插件或系统指令的消息，复用个人伴侣的角色卡、跨端记忆与额度控制。
    pub(crate) async fn handle_web_chat_event(
        &self,
        ctx: &RuntimeEventContext<'_>,
        account_id: i64,
    ) {
        let raw_text = ctx
            .domain_event
            .as_ref()
            .and_then(|event| event.message())
            .map(|message| {
                message
                    .segments
                    .iter()
                    .filter_map(|segment| match segment {
                        MessageSegment::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .unwrap_or_else(|| {
                ctx.event
                    .get("raw_message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            });
        let trimmed = raw_text.trim();
        let text = if let Some(rest) = strip_ai_prefix(trimmed) {
            if rest.is_empty() {
                ctx.send_reply_text("请在 /ai 后输入想聊的内容。".into(), "Assistant")
                    .await;
                return;
            }
            rest
        } else {
            trimmed
        };
        if text.is_empty() {
            return;
        }
        let config = self.config.snapshot().await;
        let settings = effective_assistant_settings(&config.ai);
        if !settings.enabled {
            ctx.send_reply_text(
                "网页 AI 对话尚未开启；你仍然可以直接发送插件或系统指令（如 nuono help）。".into(),
                "Assistant",
            )
            .await;
            return;
        }
        if config.ai.is_account_blocked(account_id) {
            ctx.send_reply_text(
                "管理员已暂停你的 AI 使用权限；你仍然可以使用插件与系统指令。".into(),
                "Assistant",
            )
            .await;
            return;
        }
        if self.reconcile_idle(account_id).await.is_err() {
            ctx.send_reply_text("上次对话结果正在核对，请稍后重试。".into(), "Assistant")
                .await;
            return;
        }
        let retry_of = if text == "重试" {
            let latest = self
                .db
                .assistant_turns(account_id)
                .await
                .ok()
                .and_then(|turns| turns.into_iter().last());
            match latest.as_ref() {
                Some(receipt) if receipt.retryable => Some(receipt.id.clone()),
                Some(receipt) if receipt.status == TurnStatus::Succeeded => {
                    let reply = receipt
                        .reply
                        .clone()
                        .unwrap_or_else(|| "暂无可重试的消息，请直接发送新的聊天内容。".into());
                    ctx.send_reply_text(reply, "Assistant").await;
                    return;
                }
                Some(receipt) if receipt.status == TurnStatus::Running => {
                    ctx.send_reply_text("助手正在回复，请等当前消息完成。".into(), "Assistant")
                        .await;
                    return;
                }
                _ => {
                    ctx.send_reply_text(
                        "暂无可重试的消息，请直接发送新的聊天内容。".into(),
                        "Assistant",
                    )
                    .await;
                    return;
                }
            }
        } else {
            None
        };
        let session_id = ctx
            .event
            .get("web_chat_session_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("session");
        let message_id = ctx
            .event
            .get("message_id")
            .and_then(|value| {
                value
                    .as_i64()
                    .map(|id| id.to_string())
                    .or_else(|| value.as_str().map(str::to_owned))
            })
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let key = format!("webchat:{session_id}:{message_id}");
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let delivery = AssistantDelivery::WebChat {
            reply: ctx.reply_context(),
            done: done_tx,
        };
        match self
            .submit(
                account_id,
                key,
                retry_of.is_none().then(|| text.to_owned()),
                retry_of,
                Some(delivery),
            )
            .await
        {
            Ok(AssistantAdmission::Accepted) => {
                let _ = done_rx.await;
            }
            Ok(AssistantAdmission::Duplicate) => {}
            Ok(AssistantAdmission::Unavailable) => {
                ctx.send_reply_text(
                    "当前账号暂无 AI 对话权限，请联系管理员；你仍然可以使用插件与系统指令。".into(),
                    "Assistant",
                )
                .await;
            }
            Ok(AssistantAdmission::Exhausted) => {
                ctx.send_reply_text(
                    "今天的 AI 对话额度已用完；你仍然可以使用插件与系统指令。".into(),
                    "Assistant",
                )
                .await;
            }
            Err(NuonoError::Conflict(message)) | Err(NuonoError::PermissionDenied(message)) => {
                ctx.send_reply_text(message, "Assistant").await;
            }
            Err(NuonoError::Validation(_)) => {
                ctx.send_reply_text("请发送 1 至 4096 字节的文字消息。".into(), "Assistant")
                    .await;
            }
            Err(error) => {
                tracing::warn!(error_kind = error.kind(), "网页伴侣助手回复失败");
                ctx.send_reply_text("AI 助手暂时无法回复，请稍后重试。".into(), "Assistant")
                    .await;
            }
        }
    }
}

/// 若消息以 `/ai` 开头（不区分大小写且后跟空白或结束），返回去掉前缀后的正文。
fn strip_ai_prefix(text: &str) -> Option<&str> {
    let prefix = text.get(..3)?;
    if !prefix.eq_ignore_ascii_case("/ai") {
        return None;
    }
    let rest = &text[3..];
    if rest.is_empty() {
        Some("")
    } else if rest.chars().next().is_some_and(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

impl Assistant {
    // 前端轮询页面视图时，可能正好遇上正在执行的控制轮次或显式隐私清理任务。
    // 此处等待该后台清理协程完成而不提前暴露隐私屏障后的数据；若清理异常中断或失败，则保持屏障拦截状态，要求显式重试清理。
    async fn wait_for_privacy_ready(&self, account_id: i64) -> Result<()> {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while self.privacy_blocked.contains(&account_id)
                && (self.busy.contains(&account_id) || self.deleting.contains(&account_id))
            {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await;
        self.ensure_privacy_ready(account_id)
    }

    fn ensure_privacy_ready(&self, account_id: i64) -> Result<()> {
        if self.privacy_blocked.contains(&account_id) {
            return Err(NuonoError::Conflict(
                "个人数据清理尚未完成，请重试清理后再聊天".into(),
            ));
        }
        Ok(())
    }

    /// 仅停止并排空当前账号的助手会话并清除个人数据，其他账号与群聊服务不受影响。
    pub async fn clear_personal_data(&self, account_id: i64, card_id: Option<&str>) -> Result<()> {
        // 一旦隐私删除日志（journal）持久化落盘，即使前端跳转页面或断开 HTTP 连接也不能中断删除流程；宿主在停机时会等待该后台任务排空。
        let service = self.clone();
        let card_id = card_id.map(str::to_owned);
        let (completed, response) = tokio::sync::oneshot::channel();
        self.runtime
            .spawn_settling_reply(move |_stop| async move {
                let result = service
                    .clear_personal_data_inner(account_id, card_id.as_deref())
                    .await;
                let _ = completed.send(result);
            })
            .await?;
        response
            .await
            .map_err(|_| NuonoError::Conflict("个人数据清理结果尚未确认，请重试".into()))?
    }

    async fn clear_personal_data_inner(
        &self,
        account_id: i64,
        card_id: Option<&str>,
    ) -> Result<()> {
        use nuono_app::companion_data::{CompanionDeletion, DeletionKind};
        if !self.active_account(account_id).await? {
            return Err(NuonoError::PermissionDenied("请使用有效账号登录".into()));
        }
        if !self.deleting.insert(account_id) {
            return Err(NuonoError::Conflict("个人数据清理正在进行".into()));
        }
        let _deleting = Turn {
            busy: self.deleting.clone(),
            account_id,
        };
        // 准入控制在获取会话忙碌锁（busy guard）的前后都会检查隐私删除标志。
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while self.busy.contains(&account_id) {
                for id in self.db.running_assistant_turn_ids(account_id).await? {
                    let _ = self
                        .runtime
                        .cancel_run(&nuono_app::assistant::assistant_run_id(account_id, &id))
                        .await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Ok::<_, NuonoError>(())
        })
        .await
        .map_err(|_| NuonoError::Conflict("当前回复仍在收尾，尚未清除，请稍后重试".into()))??;
        let scopes = if let Some(id) = card_id {
            self.db.companion_card(account_id, id).await?;
            vec![companion_scope_key(account_id, id)]
        } else {
            let mut scopes = self
                .db
                .list_companion_cards(account_id)
                .await?
                .iter()
                .map(|card| companion_scope_key(account_id, &card.id))
                .collect::<Vec<_>>();
            // 一并纳入引入角色卡机制之前的早期旧版个人会话作用域。
            scopes.push(nuono_app::agent::assistant_chat_key(account_id));
            scopes
        };
        let entry = CompanionDeletion {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: account_id,
            scope_keys: scopes,
            through: chrono::Utc::now(),
            kind: if card_id.is_some() {
                DeletionKind::Conversation
            } else {
                DeletionKind::All
            },
        };
        let retrying_incomplete_deletion = self.privacy_blocked.contains(&account_id);
        self.journal.append(&entry).await?;
        self.privacy_blocked.insert(account_id);
        if retrying_incomplete_deletion {
            // 补偿重放此前已写入删除日志但数据库提交未完成的残留操作；常规清理仅处理当前账号自身的日志条目，不会重放所有用户的历史记录。
            self.journal.replay(&self.db).await?;
        } else {
            self.db.apply_companion_deletion(&entry).await?;
        }
        self.privacy_blocked.remove(&account_id);
        Ok(())
    }
}

impl Assistant {
    async fn interrupt_maintenance(&self, account_id: i64) -> Result<Option<Turn>> {
        if !self.maintenance.contains(&account_id) {
            return Ok(None);
        }
        if !self.foreground_waiting.insert(account_id) {
            return Err(NuonoError::Conflict(
                "正在接收上一条消息，请稍后发送".into(),
            ));
        }
        let guard = Turn {
            busy: self.foreground_waiting.clone(),
            account_id,
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while self.busy.contains(&account_id) {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| NuonoError::Conflict("后台整理仍在收尾，请稍后重试".into()))?;
        Ok(Some(guard))
    }

    async fn redeliver_success(
        &self,
        account_id: i64,
        id: &str,
        key: &str,
        delivery: PlatformDelivery,
    ) -> Option<&'static str> {
        if self.ensure_privacy_ready(account_id).is_err() || self.deleting.contains(&account_id) {
            return Some("个人数据正在清理，暂时不能重投旧回复。");
        }
        let _waiting = match self.interrupt_maintenance(account_id).await {
            Ok(waiting) => waiting,
            Err(_) => return Some("助手正在回复，请稍后重试。"),
        };
        if !self.busy.insert(account_id) {
            return Some("助手正在回复，请稍后重试。");
        }
        let _guard = Turn {
            busy: self.busy.clone(),
            account_id,
        };
        if self.ensure_privacy_ready(account_id).is_err() || self.deleting.contains(&account_id) {
            return Some("个人数据正在清理，暂时不能重投旧回复。");
        }
        let receipt = self
            .db
            .assistant_turns(account_id)
            .await
            .ok()?
            .into_iter()
            .find(|turn| turn.id == id && turn.status == TurnStatus::Succeeded)?;
        // 上下文批次号（context_epoch）已切换时，旧上下文留下的历史归档记录绝不能作为新的 QQ 回复下发。
        if self.db.companion_epoch(&receipt.scope_key).await.ok() != Some(receipt.context_epoch) {
            return Some("这条回复属于已结束的上下文，请发送新的消息。");
        }
        self.deliver_receipt(account_id, &receipt, key, delivery)
            .await;
        None
    }
}
