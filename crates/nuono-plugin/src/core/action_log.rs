//! 发送日志的来源与结果；不参与授权、路由或重试决策。

use std::future::Future;

/// 当前发送的插件来源，仅用于日志。
pub fn current_plugin() -> Option<String> {
    super::log_context::current_plugin()
}

/// 将日志来源带入一次发送及其宿主运行时任务。
pub async fn scope<F: Future>(plugin: Option<String>, future: F) -> F::Output {
    super::log_context::scope(plugin.or_else(current_plugin), future).await
}

/// 平台请求实际达到的状态，不代表接收者已收到或读过消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SendState {
    Queued,
    Submitted,
    Confirmed,
    Failed,
    Timeout,
    Unconfirmed,
    Cancelled,
}

impl SendState {
    /// 结构化日志使用的稳定状态名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Submitted => "submitted",
            Self::Confirmed => "confirmed",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Unconfirmed => "unconfirmed",
            Self::Cancelled => "cancelled",
        }
    }
}
