//! 个人对话的持久化准入、结算与中断恢复。

use chrono::Utc;
use nuono_app::{
    agent::{companion_history_key, companion_scope_key},
    assistant::{
        AssistantSettings, AssistantTurn, MAX_MESSAGE_BYTES, TurnFailure, assistant_run_id,
    },
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};

use super::{DAY_SECONDS, Database, companion_epoch_on};
use crate::{
    conversation_store::{
        AI_RUN_STATUS_SUCCEEDED, CompletedAiRun, DIRECTION_INBOUND, DIRECTION_OUTBOUND, NewMessage,
        ROLE_ASSISTANT, ROLE_USER, append_message_on, ensure_session_conversation_on,
        record_completed_ai_run_on,
    },
    entity::{
        account_assistant as accounts, agent_runs, ai_personas, assistant_turns as turns,
        users::orm as users,
    },
    error::{NuonoError, Result},
};

/// 一次发送或显式重试；重试正文从该账号的原回执读取。
#[derive(Debug)]
pub struct AssistantTurnRequest<'a> {
    pub request_key: &'a str,
    pub text: Option<&'a str>,
    pub retry_of: Option<&'a str>,
    pub sender: Option<&'a nuono_domain::IdentityRef>,
}

#[derive(Debug)]
pub enum AssistantTurnAdmission {
    Accepted(AssistantTurn),
    Duplicate,
    Unavailable,
    Exhausted,
}

fn projection(row: turns::Model, retried: bool) -> Result<AssistantTurn> {
    let retryable =
        !row.control && !row.compacted && !row.redacted && !retried && row.failure.is_some();
    Ok(AssistantTurn {
        control: row.control,
        compacted: row.compacted,
        card_id: row.card_id,
        scope_key: row.scope_key,
        context_epoch: row.context_epoch,
        prompt_snapshot: row.prompt_snapshot,
        id: row.id,
        text: row.text,
        status: serde_json::from_value(serde_json::Value::String(row.status))?,
        failure: row
            .failure
            .map(|code| serde_json::from_value(serde_json::Value::String(code)))
            .transpose()?,
        reply: row.reply,
        delivery: serde_json::from_value(serde_json::Value::String(row.delivery))?,
        retryable,
        retry_of: row.retry_of,
        created_at: crate::clock::load_time(row.created_at),
    })
}

impl Database {
    pub async fn assistant_request_seen(&self, user_id: i64, key: &str) -> Result<bool> {
        if !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        Ok(turns::Entity::find()
            .filter(turns::Column::UserId.eq(user_id))
            .filter(
                sea_orm::Condition::any()
                    .add(turns::Column::RequestKey.eq(key))
                    .add(turns::Column::DeliveryKey.eq(key)),
            )
            .one(self.orm())
            .await?
            .is_some())
    }

    pub async fn assistant_request_replayed(
        &self,
        user_id: i64,
        key: &str,
        text: Option<&str>,
        retry_of: Option<&str>,
    ) -> Result<bool> {
        if !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        let Some(row) = turns::Entity::find()
            .filter(turns::Column::UserId.eq(user_id))
            .filter(turns::Column::RequestKey.eq(key))
            .one(self.orm())
            .await?
        else {
            return Ok(false);
        };
        if !row.redacted
            && !row.compacted
            && !(row.control && row.status != "running")
            && (row.retry_of.as_deref() != retry_of
                || text.is_some_and(|text| text.trim() != row.text))
        {
            return Err(NuonoError::Conflict(
                "这次请求编号已用于其他消息，请刷新后重试".into(),
            ));
        }
        Ok(true)
    }

    /// 在同一事务中登记请求与预扣额度。同一请求编号永远不会自动再次执行。
    pub async fn start_assistant_turn(
        &self,
        user_id: i64,
        settings: &AssistantSettings,
        input: AssistantTurnRequest<'_>,
    ) -> Result<AssistantTurnAdmission> {
        use AssistantTurnAdmission::*;
        let text = input.text.map(str::trim);
        let control =
            text.is_some_and(nuono_app::agent::companion_memory::is_memory_control_request);
        if input.request_key.is_empty()
            || input.request_key.len() > 256
            || text.is_some() == input.retry_of.is_some()
            || text.is_some_and(|text| text.is_empty() || text.len() > MAX_MESSAGE_BYTES)
            || input.retry_of.is_some_and(|id| id.len() > 64)
        {
            return Err(NuonoError::Validation(
                "请发送有效消息或选择一条失败消息重试".into(),
            ));
        }
        // 首次初始化默认角色卡与切换角色卡共用同一把账号行锁；
        // 在下方的写事务与准入锁内会再次读取当前选中的角色卡，确保其提示词快照与记忆作用域前后一致。
        if users::Entity::find_by_id(user_id)
            .filter(users::Column::Status.eq(super::super::STATUS_ACTIVE))
            .one(self.orm())
            .await?
            .is_none()
        {
            return Ok(Unavailable);
        }
        self.ensure_default_companion_card(user_id).await?;
        let tx = self.begin_write().await?;
        if !self.active_assistant_user(&tx, user_id).await? {
            return Ok(Unavailable);
        }
        let account = accounts::Entity::find_by_id(user_id).one(&tx).await?;
        if let Some(sender) = input.sender
            && !super::super::platform_identities::identity_binding_on(&tx, user_id, sender).await?
        {
            return Ok(Unavailable);
        }
        if let Some(previous) = turns::Entity::find()
            .filter(turns::Column::UserId.eq(user_id))
            .filter(turns::Column::RequestKey.eq(input.request_key))
            .one(&tx)
            .await?
        {
            if !previous.redacted
                && !previous.compacted
                && !(previous.control && previous.status != "running")
                && (previous.retry_of.as_deref() != input.retry_of
                    || text.is_some_and(|text| text != previous.text))
            {
                return Err(NuonoError::Conflict(
                    "这次请求编号已用于其他消息，请刷新后重试".into(),
                ));
            }
            return Ok(Duplicate);
        }
        let card_id = account
            .as_ref()
            .and_then(|a| a.active_card_id.as_deref())
            .ok_or_else(|| NuonoError::Conflict("请先选择角色".into()))?;
        let card_row = ai_personas::Entity::find_by_id(card_id)
            .filter(ai_personas::Column::OwnerUserId.eq(user_id))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("角色不存在".into()))?;
        let card = nuono_app::companion::CompanionCard {
            id: card_row.id,
            user_id,
            name: card_row.name,
            description: card_row.description,
            system_prompt: card_row.system_prompt,
            user_address: card_row.user_address,
            avatar_filename: card_row.avatar_filename,
            portrait_filename: card_row.portrait_filename,
            reference_audio_filename: card_row.reference_audio_filename,
            reference_text: card_row.reference_text,
            language: card_row.language,
            revision: card_row.revision,
            updated_at: crate::clock::format_wire_ts(card_row.updated_at),
        };
        let scope_key = companion_scope_key(user_id, card_id);
        let context_epoch = match &account {
            Some(row) => row.context_epoch(&scope_key)?,
            None => 0,
        };
        let text = if let Some(id) = input.retry_of {
            let original = turns::Entity::find_by_id(id)
                .filter(turns::Column::UserId.eq(user_id))
                .one(&tx)
                .await?
                .ok_or_else(|| NuonoError::NotFound("找不到可重试的消息".into()))?;
            if turns::Entity::find()
                .filter(turns::Column::UserId.eq(user_id))
                .filter(turns::Column::RetryOf.eq(id))
                .one(&tx)
                .await?
                .is_some()
            {
                return Ok(Duplicate);
            }
            if original.control
                || original.compacted
                || original.redacted
                || original.scope_key != scope_key
                || original.context_epoch != context_epoch
            {
                return Err(NuonoError::Conflict(
                    "原消息所属角色或上下文已变化，不能重试".into(),
                ));
            }
            if !matches!(original.status.as_str(), "failed" | "interrupted") {
                return Err(NuonoError::Conflict("这条消息无需重新生成回复".into()));
            }
            original.text
        } else {
            text.unwrap_or_default().to_owned()
        };
        if turns::Entity::find()
            .filter(turns::Column::UserId.eq(user_id))
            .filter(turns::Column::Status.eq("running"))
            .one(&tx)
            .await?
            .is_some()
        {
            return Err(NuonoError::Conflict(
                "助手正在回复，请等当前消息完成".into(),
            ));
        }
        let now = Utc::now();
        let limit = account.as_ref().map_or_else(
            || settings.effective_limit(None, now),
            |row| row.limit(settings, now),
        );
        if limit == 0 && !control {
            return Ok(Unavailable);
        }
        let day = now.timestamp().div_euclid(DAY_SECONDS);
        let mut requests = account
            .as_ref()
            .map(|row| row.requests(day))
            .transpose()?
            .unwrap_or_default();
        if requests.len() >= limit as usize && !control {
            return Ok(Exhausted);
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        if !control {
            requests.push(format!("turn:{id}"));
        }
        let update = accounts::ActiveModel {
            user_id: Set(user_id),
            usage_day: Set(day),
            request_ids: Set(serde_json::to_string(&requests)?),
            ..Default::default()
        };
        if account.is_some() {
            update.update(&tx).await?;
        } else {
            update.insert(&tx).await?;
        }
        let conversation =
            ensure_session_conversation_on(&tx, &companion_history_key(&scope_key, context_epoch))
                .await?;
        let row = turns::ActiveModel {
            id: Set(id),
            user_id: Set(user_id),
            conversation_id: Set(Some(conversation.id)),
            card_id: Set(card.id.clone()),
            scope_key: Set(scope_key),
            context_epoch: Set(context_epoch),
            prompt_snapshot: Set(card.prompt()),
            redacted: Set(false),
            control: Set(control),
            compacted: Set(false),
            request_key: Set(input.request_key.to_owned()),
            text: Set(text),
            retry_of: Set(input.retry_of.map(str::to_owned)),
            status: Set("running".into()),
            failure: Set(None),
            reply: Set(None),
            quota_day: Set(day),
            delivery: Set(if input.sender.is_some() {
                "pending"
            } else {
                "none"
            }
            .into()),
            delivery_key: Set(None),
            created_at: Set(crate::clock::store_time(now)),
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(Accepted(projection(row, false)?))
    }

    /// 最近四十轮仅属于此账号；重试过的失败回执不会再次提供生成入口。
    pub async fn assistant_turns(&self, user_id: i64) -> Result<Vec<AssistantTurn>> {
        let Some(card) = self.active_companion_card(user_id).await? else {
            return Ok(Vec::new());
        };
        self.assistant_turns_for_card(user_id, &card.id).await
    }

    pub async fn assistant_turns_for_card(
        &self,
        user_id: i64,
        card_id: &str,
    ) -> Result<Vec<AssistantTurn>> {
        self.companion_card(user_id, card_id).await?;
        let epoch = self
            .companion_epoch(&companion_scope_key(user_id, card_id))
            .await?;
        let mut rows = turns::Entity::find()
            .filter(turns::Column::UserId.eq(user_id))
            .filter(turns::Column::CardId.eq(card_id))
            .filter(turns::Column::Redacted.eq(false))
            .order_by_desc(turns::Column::CreatedAt)
            .order_by_desc(turns::Column::Id)
            .limit(40)
            .all(self.orm())
            .await?;
        let retried = turns::Entity::find()
            .select_only()
            .column(turns::Column::RetryOf)
            .filter(turns::Column::UserId.eq(user_id))
            .filter(turns::Column::RetryOf.is_in(rows.iter().map(|row| row.id.clone())))
            .into_tuple::<String>()
            .all(self.orm())
            .await?;
        rows.reverse();
        rows.into_iter()
            .map(|row| {
                let retry = retried.contains(&row.id) || row.context_epoch != epoch;
                projection(row, retry)
            })
            .collect()
    }

    /// 从执行检查点结算。成功历史、失败退款和回执状态在同一事务发布。
    pub async fn settle_assistant_turn(
        &self,
        user_id: i64,
        id: &str,
        fallback: TurnFailure,
    ) -> Result<Option<AssistantTurn>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let tx = self.begin_write().await?;
        let mut user = users::Entity::find_by_id(user_id);
        if self.backend() == sea_orm::DbBackend::Postgres {
            user = user.lock_exclusive();
        }
        if user.one(&tx).await?.is_none() {
            return Ok(None);
        }
        let Some(row) = turns::Entity::find_by_id(id)
            .filter(turns::Column::UserId.eq(user_id))
            .one(&tx)
            .await?
        else {
            return Ok(None);
        };
        if row.status != "running" {
            return Ok(Some(projection(row, false)?));
        }
        let run = agent_runs::Entity::find_by_id(assistant_run_id(user_id, id))
            .one(&tx)
            .await?;
        let current_epoch = companion_epoch_on(&tx, user_id, &row.scope_key).await?;
        let current = !row.redacted && row.context_epoch == current_epoch;
        let reply = run
            .as_ref()
            .filter(|_| current)
            .filter(|run| run.status == "completed")
            .and_then(|run| {
                serde_json::from_str::<serde_json::Value>(&run.state_json)
                    .ok()?
                    .get("reply")?
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                    .map(str::to_owned)
            });
        let failure = if reply.is_some() {
            None
        } else {
            Some(if !current {
                TurnFailure::Interrupted
            } else {
                match run.as_ref().map(|run| run.status.as_str()) {
                    Some("timed_out" | "budget_exceeded") => TurnFailure::TimedOut,
                    Some("failed" | "completed") => TurnFailure::ModelFailed,
                    Some("cancelled" | "interrupted") => TurnFailure::Interrupted,
                    Some("rejected") => TurnFailure::Unavailable,
                    _ => fallback,
                }
            })
        };
        let mut resolved_conversation_id = row.conversation_id.clone();
        if let Some(reply) = &reply {
            let conv = match resolved_conversation_id.as_deref() {
                Some(existing_id) => {
                    match crate::entity::conversations::Entity::find_by_id(existing_id)
                        .one(&tx)
                        .await?
                    {
                        Some(found) => found,
                        None => {
                            ensure_session_conversation_on(
                                &tx,
                                &companion_history_key(&row.scope_key, row.context_epoch),
                            )
                            .await?
                        }
                    }
                }
                None => {
                    ensure_session_conversation_on(
                        &tx,
                        &companion_history_key(&row.scope_key, row.context_epoch),
                    )
                    .await?
                }
            };
            resolved_conversation_id = Some(conv.id.clone());
            let now_ms = crate::clock::db_now().timestamp_millis();
            let trigger_message_id = append_message_on(
                &tx,
                NewMessage {
                    conversation_id: Some(conv.id.clone()),
                    platform: conv.platform.clone(),
                    bot_instance_id: None,
                    native_message_id: None,
                    reply_to_native_message_id: None,
                    sender_user_id: Some(user_id),
                    sender_identity_id: None,
                    native_sender_id: None,
                    native_group_id: None,
                    sender_nickname: None,
                    direction: DIRECTION_INBOUND.to_owned(),
                    role: ROLE_USER.to_owned(),
                    text_content: row.text.clone(),
                    content_blocks_json: "[]".to_owned(),
                    raw_payload_json: None,
                    occurred_at_ms: now_ms,
                },
            )
            .await?;
            let reply_message_id = append_message_on(
                &tx,
                NewMessage {
                    conversation_id: Some(conv.id.clone()),
                    platform: conv.platform,
                    bot_instance_id: None,
                    native_message_id: None,
                    reply_to_native_message_id: None,
                    sender_user_id: None,
                    sender_identity_id: None,
                    native_sender_id: None,
                    native_group_id: None,
                    sender_nickname: None,
                    direction: DIRECTION_OUTBOUND.to_owned(),
                    role: ROLE_ASSISTANT.to_owned(),
                    text_content: reply.clone(),
                    content_blocks_json: "[]".to_owned(),
                    raw_payload_json: None,
                    occurred_at_ms: now_ms,
                },
            )
            .await?;
            let latency_ms =
                (now_ms - row.created_at.timestamp_millis()).clamp(0, i32::MAX as i64) as i32;
            record_completed_ai_run_on(
                &tx,
                CompletedAiRun {
                    id: Some(row.id.clone()),
                    conversation_id: conv.id,
                    trigger_message_id,
                    reply_message_id,
                    user_id: Some(user_id),
                    provider: String::new(),
                    model: String::new(),
                    status: AI_RUN_STATUS_SUCCEEDED.to_owned(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    latency_ms,
                    steps_json: "[]".to_owned(),
                    error_message: None,
                    created_at_ms: row.created_at.timestamp_millis(),
                    finished_at_ms: Some(now_ms),
                },
            )
            .await?;
        } else if let Some(account) = accounts::Entity::find_by_id(user_id)
            .filter(accounts::Column::UsageDay.eq(row.quota_day))
            .one(&tx)
            .await?
        {
            let mut requests = account.requests(row.quota_day)?;
            requests.retain(|key| key != &format!("turn:{id}"));
            accounts::ActiveModel {
                user_id: Set(user_id),
                request_ids: Set(serde_json::to_string(&requests)?),
                ..Default::default()
            }
            .update(&tx)
            .await?;
        }
        let updated = turns::ActiveModel {
            id: Set(id.into()),
            conversation_id: Set(resolved_conversation_id),
            status: Set(if failure == Some(TurnFailure::Interrupted) {
                "interrupted"
            } else if failure.is_some() {
                "failed"
            } else {
                "succeeded"
            }
            .into()),
            failure: Set(failure
                .map(|failure| {
                    serde_json::to_value(failure)
                        .map(|value| value.as_str().unwrap_or_default().to_owned())
                })
                .transpose()?),
            reply: Set(reply),
            ..Default::default()
        }
        .update(&tx)
        .await?;
        tx.commit().await?;
        Ok(Some(projection(updated, false)?))
    }

    /// 只在此账号没有本地执行者时调用；不重新调用模型或自动发送回复。
    pub async fn reconcile_assistant_account(&self, user_id: i64) -> Result<()> {
        if !self.is_ai_module_installed().await? {
            return Ok(());
        }
        for row in turns::Entity::find()
            .filter(turns::Column::UserId.eq(user_id))
            .filter(turns::Column::Status.eq("running"))
            .all(self.orm())
            .await?
        {
            self.settle_assistant_turn(user_id, &row.id, TurnFailure::Interrupted)
                .await?;
        }
        Ok(())
    }

    /// 在任何入站请求之前返还上次进程未完成对话的额度，保留已落盘的成功结果。
    pub async fn reconcile_assistant_turns_before_start(&self) -> Result<u64> {
        if !self.is_ai_module_installed().await? {
            return Ok(0);
        }
        let mut recovered = 0;
        loop {
            let rows = turns::Entity::find()
                .filter(turns::Column::Status.eq("running"))
                .limit(100)
                .all(self.orm())
                .await?;
            if rows.is_empty() {
                return Ok(recovered);
            }
            for row in rows {
                self.settle_assistant_turn(row.user_id, &row.id, TurnFailure::Interrupted)
                    .await?;
                recovered += 1;
            }
        }
    }

    /// 投递前先登记未知结果，避免当前投递的重复请求立即再次发送。
    pub async fn begin_assistant_delivery(
        &self,
        user_id: i64,
        id: &str,
        key: &str,
    ) -> Result<bool> {
        if !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        let result = turns::Entity::update_many()
            .col_expr(
                turns::Column::Delivery,
                sea_orm::sea_query::Expr::value("unknown"),
            )
            .col_expr(
                turns::Column::DeliveryKey,
                sea_orm::sea_query::Expr::value(key),
            )
            .filter(turns::Column::UserId.eq(user_id))
            .filter(turns::Column::Id.eq(id))
            .filter(turns::Column::Redacted.eq(false))
            .filter(
                sea_orm::Condition::any()
                    .add(turns::Column::DeliveryKey.is_null())
                    .add(turns::Column::DeliveryKey.ne(key)),
            )
            .exec(self.orm())
            .await?;
        Ok(result.rows_affected == 1)
    }

    pub async fn confirm_assistant_delivery(
        &self,
        user_id: i64,
        id: &str,
        key: &str,
    ) -> Result<()> {
        if !self.is_ai_module_installed().await? {
            return Ok(());
        }
        turns::Entity::update_many()
            .col_expr(
                turns::Column::Delivery,
                sea_orm::sea_query::Expr::value("sent"),
            )
            .filter(turns::Column::UserId.eq(user_id))
            .filter(turns::Column::Id.eq(id))
            .filter(turns::Column::DeliveryKey.eq(key))
            .exec(self.orm())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/db/assistant_turns.rs"]
mod tests;
