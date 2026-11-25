//! 账号合并的插件参与方接口。参与方仅借用宿主开启的数据库事务，事务的提交与回滚由宿主统一控制。

use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock, RwLock},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountMergeContext {
    pub operation_id: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub source_user_id: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub target_user_id: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MergeStateRecord {
    pub collection: String,
    pub key: String,
    pub value_version: i32,
    pub value: serde_json::Value,
}

/// 合并预检阶段的只读数据访问接口，宿主仅允许读取该插件自身注册的数据表。
#[async_trait::async_trait]
pub trait AccountMergeRead: Send + Sync {
    fn backend(&self) -> sea_orm::DbBackend;
    async fn account_state(&self, user_id: i64) -> Result<Vec<MergeStateRecord>>;
    async fn query(&self, statement: sea_orm::Statement) -> Result<Vec<sea_orm::QueryResult>>;
}

/// 合并执行阶段的事务写入接口。不暴露底层连接、事务控制（开启/提交/回滚）、文件或网络访问能力。
#[async_trait::async_trait]
pub trait AccountMergeTransaction: AccountMergeRead {
    async fn execute(&self, statement: sea_orm::Statement) -> Result<u64>;
    async fn put_account_state(&self, user_id: i64, record: MergeStateRecord) -> Result<()>;
    async fn delete_account_state(&self, user_id: i64, collection: &str, key: &str) -> Result<()>;
}

/// 插件侧的自定义账号合并处理器，用于配合持久化的账号引用声明执行复杂合并逻辑。
#[async_trait::async_trait]
pub trait AccountMergeParticipant: Send + Sync {
    /// 合并方案的版本号；每当预检输出格式或写入逻辑发生变化时应递增该版本号。
    fn version(&self) -> i64;
    /// 预检阶段（只读）：基于当前数据快照生成确定性的合并执行计划，计划中不得包含敏感凭据。
    async fn preflight(
        &self,
        context: &AccountMergeContext,
        data: &dyn AccountMergeRead,
    ) -> Result<serde_json::Value>;
    /// 执行阶段：在宿主主事务中调用一次，与核心账号数据迁移及合并完成回执处于同一事务内。
    async fn apply(
        &self,
        context: &AccountMergeContext,
        plan: &serde_json::Value,
        transaction: &dyn AccountMergeTransaction,
    ) -> Result<()>;
}

fn registry() -> &'static RwLock<BTreeMap<String, Arc<dyn AccountMergeParticipant>>> {
    static REGISTRY: OnceLock<RwLock<BTreeMap<String, Arc<dyn AccountMergeParticipant>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

pub fn register_account_merge_participant(
    plugin_id: &str,
    participant: Arc<dyn AccountMergeParticipant>,
) -> Result<()> {
    if plugin_id.is_empty() || plugin_id.len() > 64 || participant.version() <= 0 {
        return Err(NuonoError::Validation(
            "账号合并参与者缺少稳定标识或版本".into(),
        ));
    }
    registry()
        .write()
        .map_err(|_| NuonoError::Internal("账号合并注册表不可用".into()))?
        .insert(plugin_id.to_owned(), participant);
    Ok(())
}

pub fn unregister_account_merge_participant(plugin_id: &str) {
    if let Ok(mut registry) = registry().write() {
        registry.remove(plugin_id);
    }
}

/// 单个合并参与者在本次事务中的执行统计摘要。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeStepSummary {
    pub participant_id: String,
    pub migrated_rows: u64,
    pub deduplicated_rows: u64,
    pub details: serde_json::Value,
}

/// 统一的账号合并参与者契约（核心子系统与外部业务插件共用）。
#[async_trait::async_trait]
pub trait UserMergeParticipant: Send + Sync {
    /// 参与者唯一标识，如 `core:platform_identities`、`plugin:billing`、`plugin:demo`。
    fn participant_id(&self) -> &'static str;

    /// 执行顺序（数值越小越早执行；核心身份层最先，插件次之）。
    fn priority(&self) -> i32 {
        100
    }

    /// 在同一数据库事务内将 `source_user_id` 的数据迁移至 `target_user_id`。
    /// 要求幂等，并自行处理唯一约束冲突。
    async fn merge_in_tx(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        source_user_id: i64,
        target_user_id: i64,
    ) -> Result<MergeStepSummary>;
}

fn user_merge_registry() -> &'static RwLock<BTreeMap<String, Arc<dyn UserMergeParticipant>>> {
    static USER_MERGE_REGISTRY: OnceLock<RwLock<BTreeMap<String, Arc<dyn UserMergeParticipant>>>> =
        OnceLock::new();
    USER_MERGE_REGISTRY.get_or_init(Default::default)
}

/// 注册统一的账号合并参与者（按 `priority` 升序编排执行）。
pub fn register_user_merge_participant(participant: Arc<dyn UserMergeParticipant>) -> Result<()> {
    let id = participant.participant_id().trim();
    if id.is_empty() || id.len() > 128 {
        return Err(NuonoError::Validation("合并参与者标识无效".into()));
    }
    user_merge_registry()
        .write()
        .map_err(|_| NuonoError::Internal("统一账号合并注册表不可用".into()))?
        .insert(id.to_owned(), participant);
    Ok(())
}

/// 注销指定标识的统一账号合并参与者。
pub fn unregister_user_merge_participant(participant_id: &str) {
    if let Ok(mut registry) = user_merge_registry().write() {
        registry.remove(participant_id);
    }
}

/// 返回当前已注册的全部统一账号合并参与者（按 `(priority, participant_id)` 确定性排序）。
pub fn registered_user_merge_participants() -> Result<Vec<Arc<dyn UserMergeParticipant>>> {
    let mut items: Vec<Arc<dyn UserMergeParticipant>> = user_merge_registry()
        .read()
        .map_err(|_| NuonoError::Internal("统一账号合并注册表不可用".into()))?
        .values()
        .cloned()
        .collect();
    items.sort_by(|a, b| {
        a.priority()
            .cmp(&b.priority())
            .then_with(|| a.participant_id().cmp(b.participant_id()))
    });
    Ok(items)
}

pub fn account_merge_participant(
    plugin_id: &str,
) -> Result<Option<Arc<dyn AccountMergeParticipant>>> {
    Ok(registry()
        .read()
        .map_err(|_| NuonoError::Internal("账号合并注册表不可用".into()))?
        .get(plugin_id)
        .cloned())
}
