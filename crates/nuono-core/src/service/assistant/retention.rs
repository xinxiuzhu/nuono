//! 个人对话历史后台维护：仅在账号空闲时运行，并与正常对话共用同一道隐私删除屏障。
use super::{Assistant, AssistantTurn, Turn, TurnDelivery, TurnStatus};
use crate::error::Result;
use nuono_app::agent::AgentContext;

impl Assistant {
    /// 每次最多巡检整理一张角色卡的历史记录；即使大模型服务暂不可用，调用方仍保留推进后的游标。
    pub async fn maintain_companion_history(&self, cursor: &mut Option<String>) -> Result<usize> {
        let settings = self.config.snapshot().await.ai.clone();
        if !settings.assistant.enabled || !settings.agent_memory_enabled {
            return Ok(0);
        }
        let raw_days = settings.assistant.raw_retention_days;
        let Some(scope) = self
            .db
            .next_companion_retention_scope(cursor.as_deref(), raw_days)
            .await?
        else {
            return Ok(0);
        };
        *cursor = Some(scope.scope_key.clone());
        let account_id = scope.user_id;
        if self.deleting.contains(&account_id)
            || self.privacy_blocked.contains(&account_id)
            || !self.busy.insert(account_id)
        {
            return Ok(0);
        }
        let _guard = Turn {
            busy: self.busy.clone(),
            account_id,
        };
        self.maintenance.insert(account_id);
        let _maintenance = Turn {
            busy: self.maintenance.clone(),
            account_id,
        };
        if self.deleting.contains(&account_id)
            || self.privacy_blocked.contains(&account_id)
            || self.foreground_waiting.contains(&account_id)
            || !self.active_account(account_id).await?
        {
            return Ok(0);
        }
        let batch = self.db.companion_retention_batch(&scope, raw_days).await?;
        let Some(first) = batch.turns.first() else {
            return Ok(0);
        };
        let turn = AssistantTurn {
            control: false,
            compacted: false,
            card_id: scope.card_id.clone(),
            scope_key: scope.scope_key.clone(),
            context_epoch: batch.epoch,
            prompt_snapshot: first.prompt_snapshot.clone(),
            id: first.id.clone(),
            text: first.text.clone(),
            status: TurnStatus::Succeeded,
            failure: None,
            reply: first.reply.clone(),
            delivery: TurnDelivery::None,
            retryable: false,
            retry_of: None,
            created_at: first.created_at.into(),
        };
        let context = AgentContext::for_companion_turn(account_id, &turn);
        let experiences = batch.experiences();
        let request = self
            .runtime
            .compress_companion_history(&context, &experiences);
        tokio::pin!(request);
        let mut interruption = tokio::time::interval(std::time::Duration::from_millis(100));
        let summary = loop {
            tokio::select! {
                biased;
                _ = interruption.tick() => {
                    if self.deleting.contains(&account_id) || self.privacy_blocked.contains(&account_id) || self.foreground_waiting.contains(&account_id) { return Ok(0); }
                }
                result = &mut request => break result?,
            }
        };
        let latest = self.config.snapshot().await.ai.clone();
        if self.deleting.contains(&account_id)
            || self.privacy_blocked.contains(&account_id)
            || self.foreground_waiting.contains(&account_id)
            || !latest.assistant.enabled
            || !latest.agent_memory_enabled
            || latest.assistant.raw_retention_days != raw_days
        {
            return Ok(0);
        }
        self.db
            .commit_companion_retention(&batch, &summary, raw_days)
            .await
    }
}
