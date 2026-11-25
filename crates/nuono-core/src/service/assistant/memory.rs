//! 编排个人长期记忆整理流程：由应用层生成记忆变更计划，存储层在核对角色卡批次号（card epoch）后原子提交。
use super::Assistant;
use crate::error::Result;
use nuono_app::{
    agent::{
        AgentContext,
        companion_memory::{MemoryIntent, MemoryPhase},
    },
    assistant::AssistantTurn,
    companion_data::{CompanionDeletion, DeletionKind},
};

impl Assistant {
    pub(super) async fn process_memory_control(
        &self,
        account_id: i64,
        receipt: &AssistantTurn,
    ) -> Result<()> {
        let ctx = AgentContext::for_companion_turn(account_id, receipt);
        let plan = match self
            .runtime
            .prepare_companion_memory(&ctx, MemoryPhase::BeforeReply)
            .await
        {
            Ok(plan) => plan,
            Err(_) => {
                return self
                    .db
                    .complete_companion_control(
                        account_id,
                        &receipt.id,
                        receipt.context_epoch,
                        "本次记忆调整未完成，原有记忆保持不变，请稍后重试。",
                        &[],
                    )
                    .await;
            }
        };
        if !matches!(plan.intent, MemoryIntent::Correct | MemoryIntent::Forget)
            || plan.removals.is_empty()
            || !self
                .db
                .companion_replacement_unambiguous(
                    account_id,
                    &receipt.id,
                    &plan.removals,
                    &plan.upserts,
                )
                .await?
        {
            return self.db.complete_companion_control(account_id, &receipt.id, receipt.context_epoch,
                "未能确定要调整的事项，记忆未更改。请明确说明要纠正或遗忘的内容，或在控制台清除全部个人聊天数据。", &[]).await;
        }
        // 本次清空指令自身的控制回执不纳入该清除时间线（cutoff）；它会单独做脱敏落库，且绝不会被用作重试、对话上下文或记忆摘要的输入源。
        let entry = CompanionDeletion {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: account_id,
            scope_keys: vec![receipt.scope_key.clone()],
            through: receipt.created_at - chrono::Duration::microseconds(1),
            kind: DeletionKind::Forget {
                memory_ids: plan.removals,
            },
        };
        self.journal.append(&entry).await?;
        self.privacy_blocked.insert(account_id);
        self.db.apply_companion_deletion(&entry).await?;
        let epoch = self.db.companion_epoch(&receipt.scope_key).await?;
        let response = if plan.intent == MemoryIntent::Correct {
            "旧记忆和相关摘要已清除，已按本次说明更新记忆，后续将从新上下文继续。"
        } else {
            "已清除该事项及相关记忆与摘要，后续不再读取此前聊天上下文；历史记录仍可在控制台查看。"
        };
        self.db
            .complete_companion_control(account_id, &receipt.id, epoch, response, &plan.upserts)
            .await?;
        self.privacy_blocked.remove(&account_id);
        Ok(())
    }

    pub(super) async fn capture_successful_experience(
        &self,
        account_id: i64,
        receipt: &AssistantTurn,
    ) {
        let ctx = AgentContext::for_companion_turn(account_id, receipt);
        let result = async {
            let plan = self
                .runtime
                .prepare_companion_memory(&ctx, MemoryPhase::AfterReply)
                .await?;
            let config = self.config.snapshot().await;
            if !self.deleting.contains(&account_id)
                && config.ai.agent_memory_enabled
                && config.ai.agent_memory_auto_extract_enabled
                && config.ai.assistant_active()
            {
                self.db
                    .save_companion_memories(
                        account_id,
                        &receipt.id,
                        receipt.context_epoch,
                        &plan.upserts,
                    )
                    .await?;
            }
            Ok::<_, crate::error::NuonoError>(())
        }
        .await;
        if result.is_err() {
            // 对话回复与计费结算已成功时不受后台记忆整理失败影响；保留的原始对话仍可在后续轮次中汇总，且不向日常日志输出对话正文或模型服务商原始报错。
            tracing::warn!(account_id, "个人记忆整理暂未完成，已保留原始对话");
        }
    }
}
