//! 绑定到已认证框架账号的文本对话能力。

use std::sync::Arc;

use crate::error::Result;

/// 账号对话中的单条消息，按持久化 ID 顺序排列。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AccountMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
}

/// 由宿主实现的账号对话底层接口。
#[async_trait::async_trait]
pub trait AccountConversationPort: Send + Sync {
    /// 读取该账号最近的对话历史消息。
    async fn recent(&self, account_id: i64) -> Result<Vec<AccountMessage>>;

    /// 结合该账号的对话上下文与长期记忆生成并保存一条文本回复。
    async fn reply(&self, account_id: i64, text: &str, system_prompt: &str) -> Result<String>;
}

/// 供当前插件使用的账号对话句柄（调用方须自行完成账号认证与鉴权）。
#[derive(Clone)]
pub struct AccountConversations(Arc<dyn AccountConversationPort>);

impl std::fmt::Debug for AccountConversations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountConversations")
            .finish_non_exhaustive()
    }
}

impl AccountConversations {
    #[doc(hidden)]
    pub fn new(port: impl AccountConversationPort + 'static) -> Self {
        Self(Arc::new(port))
    }

    /// 读取插件已完成认证与鉴权的账号的最近对话消息。
    ///
    /// # Errors
    /// 当账号或插件未激活、或底层存储不可用时返回错误。
    pub async fn recent(&self, account_id: i64) -> Result<Vec<AccountMessage>> {
        self.0.recent(account_id).await
    }

    /// 提交一轮文本对话（调用前须由插件完成对该账号的认证与鉴权）。
    ///
    /// # Errors
    /// 当输入无效、该账号正有对话在处理中、或 AI 服务不可用时返回错误。
    pub async fn reply(&self, account_id: i64, text: &str, system_prompt: &str) -> Result<String> {
        self.0.reply(account_id, text, system_prompt).await
    }
}
