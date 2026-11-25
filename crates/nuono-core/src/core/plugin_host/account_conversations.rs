use std::sync::{Arc, RwLock, Weak};

use nuono_app::agent::{account_chat_key, input::HistoryMessage};
use nuono_plugin::agent::account::{AccountConversationPort, AccountConversations, AccountMessage};

use super::{PluginHost, PluginTaskScope};
use crate::agent::{AgentContext, AgentRuntime};
use crate::db::Database;
use crate::error::{NuonoError, Result};

impl PluginHost {
    pub(super) fn account_conversations(
        &self,
        plugin_name: &str,
        scope: &Arc<PluginTaskScope>,
    ) -> AccountConversations {
        // 插件加载可能发生在 EventKernel 构造 Agent 运行时之前，因此通过宿主句柄按需访问会话记录。
        AccountConversations::new(HostAccountConversations {
            runtime: Arc::downgrade(&self.agent_runtime),
            db: self.db.clone(),
            namespace: plugin_name.to_owned(),
            scope: Arc::downgrade(scope),
        })
    }
}

struct HostAccountConversations {
    runtime: Weak<RwLock<Option<AgentRuntime>>>,
    db: Database,
    namespace: String,
    scope: Weak<PluginTaskScope>,
}

impl HostAccountConversations {
    async fn authorize(&self, account_id: i64) -> Result<()> {
        if self.scope.upgrade().is_none_or(|scope| scope.is_closed())
            || !self
                .db
                .find_user_by_id(account_id)
                .await?
                .is_some_and(|user| user.status == "active")
        {
            return Err(NuonoError::PermissionDenied("账号对话不可用".into()));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl AccountConversationPort for HostAccountConversations {
    async fn recent(&self, account_id: i64) -> Result<Vec<AccountMessage>> {
        self.authorize(account_id).await?;
        let key = account_chat_key(&self.namespace, account_id);
        let rows = self.db.recent_ai_conversations(&key, 80).await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let (role, content) = if row.role == "assistant" {
                    (row.role, row.content)
                } else {
                    (
                        "user".into(),
                        HistoryMessage::decode(&row.role, &row.content)?.content,
                    )
                };
                Some(AccountMessage {
                    id: row.id,
                    role,
                    content,
                })
            })
            .collect())
    }

    async fn reply(&self, account_id: i64, text: &str, system_prompt: &str) -> Result<String> {
        self.authorize(account_id).await?;
        if system_prompt.trim().is_empty() || system_prompt.len() > 8192 {
            return Err(NuonoError::Validation("角色设定须为 1 至 8192 字节".into()));
        }
        let ctx = AgentContext::for_account_chat(
            &self.namespace,
            account_id,
            text.to_owned(),
            system_prompt.to_owned(),
        );
        let runtime = self
            .runtime
            .upgrade()
            .and_then(|slot| slot.read().unwrap_or_else(|err| err.into_inner()).clone())
            .ok_or_else(|| NuonoError::Internal("宿主对话运行时尚未就绪".into()))?;
        let task = runtime
            .submit_text(ctx)
            .await?
            .ok_or_else(|| NuonoError::PermissionDenied("AI 对话未启用或未获授权".into()))?;
        Ok(task.await?.content)
    }
}
