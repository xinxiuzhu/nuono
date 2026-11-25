//! 可选的扫码绑定能力；协议状态和凭据只在宿主进程内传递。
use async_trait::async_trait;
use serde_json::Value;

/// 扫码绑定的当前结果。完成配置可包含密钥，宿主不得直接序列化它。
#[non_exhaustive]
pub enum QrLoginStatus {
    Pending,
    Expired,
    Complete(Value),
}

impl std::fmt::Debug for QrLoginStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pending => "Pending",
            Self::Expired => "Expired",
            Self::Complete(_) => "Complete([REDACTED])",
        })
    }
}

/// 一次绑定任务，持有提供商的临时密钥，丢弃即结束本地会话。
#[async_trait]
pub trait QrLoginSession: Send + Sync {
    /// 用户扫描的 HTTPS 授权地址，不应包含 AppSecret。
    fn qr_url(&self) -> &str;
    /// 查询授权结果；不能启动实例或持久化凭据。
    async fn poll(&mut self) -> crate::Result<QrLoginStatus>;
    /// 扫码成功后从平台侧获取到的机器人资料（昵称与头像），获取失败时返回空资料。
    fn profile(&self) -> crate::BotProfile {
        crate::BotProfile::default()
    }
}
