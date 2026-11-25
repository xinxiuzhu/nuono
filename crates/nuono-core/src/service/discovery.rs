//! 群同步时的归属解析扩展点。
//!
//! 默认实现 [`NoopDiscovery`] 让新群保持未指派状态。嵌入方可按上报机器人
//! 解析新群的初始归属。

use std::sync::{Arc, OnceLock};

use crate::db::Database;
use crate::error::Result;

/// 决定「上报群列表的 bot 属于哪个用户」的解析。
#[async_trait::async_trait]
pub trait GroupDiscoveryHook: Send + Sync {
    /// 返回 bot 的所属用户；`None` 表示本次同步不写归属。框架每次同步只问一次。
    async fn bot_owner(&self, db: &Database, bot_id: &str) -> Result<Option<i64>>;
}

/// 开源默认：不做自动认领，所有新群保持无主。
#[derive(Debug, Default)]
pub struct NoopDiscovery;

#[async_trait::async_trait]
impl GroupDiscoveryHook for NoopDiscovery {
    async fn bot_owner(&self, _db: &Database, _bot_id: &str) -> Result<Option<i64>> {
        Ok(None)
    }
}

static HOOK: OnceLock<Arc<dyn GroupDiscoveryHook>> = OnceLock::new();

/// 注入全局归属解析。与权益判定同理只允许注册一次。
pub fn set_group_discovery_hook(hook: impl GroupDiscoveryHook + 'static) {
    let _ = HOOK.set(Arc::new(hook));
}

/// 当前生效的解析。
pub async fn resolve_bot_owner(db: &Database, bot_id: &str) -> Result<Option<i64>> {
    HOOK.get_or_init(|| Arc::new(NoopDiscovery))
        .bot_owner(db, bot_id)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_default_never_claims_ownership() {
        let db = crate::test_support::test_db(2).await;
        // 开源构建没有 bot 登记：任何 self_id 都不解析出主人。
        assert_eq!(resolve_bot_owner(&db, "123456").await.unwrap(), None);
        db.shutdown().await.unwrap();
    }
}
