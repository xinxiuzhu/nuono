//! 部署级服务策略与统一功能权益模型。框架维护开源授权层（`nuono_entitlements`），
//! 服务周期、订单与价格由外部计费插件维护并通过事务接口写入授权层。
//!
//! 开源构建默认不注册任何策略提供者，所有群默认开放且不要求个人助手授权。
use std::sync::{Arc, OnceLock};

/// 权益主体类型：群组。
pub const SUBJECT_GROUP: &str = "group";
/// 权益主体类型：个人账号。
pub const SUBJECT_USER: &str = "user";

/// 标准功能权益键：群机器人准入权限。
pub const FEATURE_GROUP_BOT_ACCESS: &str = "group:bot_access";
/// 标准功能权益键：网页端个人 AI 对话权限。
pub const FEATURE_USER_WEB_AI: &str = "user:web_ai";
/// 标准功能权益键：IM 平台私聊机器人权限。
pub const FEATURE_USER_PRIVATE_BOT: &str = "user:private_bot";

/// 权益来源：计费插件自动同步。
pub const SOURCE_BILLING: &str = "billing";
/// 权益来源：站长手动授权。
pub const SOURCE_MANUAL: &str = "manual";
/// 权益来源：试用授权。
pub const SOURCE_TRIAL: &str = "trial";

/// 权益状态：生效中。
pub const STATUS_ACTIVE: &str = "active";
/// 权益状态：已撤销。
pub const STATUS_REVOKED: &str = "revoked";

/// 运行时功能权益记录（对应核心表 `nuono_entitlements`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntitlementRecord {
    pub id: i64,
    pub subject_type: String,
    pub subject_id: i64,
    pub feature_key: String,
    pub source: String,
    pub source_ref: Option<String>,
    pub status: String,
    pub starts_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub limits_json: String,
    pub updated_at_ms: i64,
}

/// 写入或刷新功能权益的输入参数（按 `(subject_type, subject_id, feature_key, source)` 幂等覆盖）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntitlementUpsert {
    pub subject_type: String,
    pub subject_id: i64,
    pub feature_key: String,
    pub source: String,
    pub source_ref: Option<String>,
    pub starts_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub limits_json: String,
}

/// 群服务权益查询上下文。
///
/// 使用 `#[non_exhaustive]` 保护未来扩展（例如按插件、按功能维度做细粒度授权），
/// 避免新增维度破坏外部商业插件的 SemVer 兼容性。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GroupEntitlementQuery {
    group_id: i64,
    plugin_id: Option<String>,
    feature: Option<String>,
}

impl GroupEntitlementQuery {
    /// 针对规范化群主键构造基础权益查询。
    pub fn for_group(group_id: i64) -> Self {
        Self {
            group_id,
            plugin_id: None,
            feature: None,
        }
    }

    /// 附加目标插件标识，供支持按插件计费的策略提供者使用。
    pub fn with_plugin(mut self, plugin_id: impl Into<String>) -> Self {
        self.plugin_id = Some(plugin_id.into());
        self
    }

    /// 附加目标功能标识，供支持按功能模块计费的策略提供者使用。
    pub fn with_feature(mut self, feature: impl Into<String>) -> Self {
        self.feature = Some(feature.into());
        self
    }

    /// 规范化群主键（框架 `groups.id`，对应具体的 `(platform, namespace, native_group_id)`）。
    pub fn group_id(&self) -> i64 {
        self.group_id
    }

    /// 可选目标插件标识。
    pub fn plugin_id(&self) -> Option<&str> {
        self.plugin_id.as_deref()
    }

    /// 可选目标功能标识。
    pub fn feature(&self) -> Option<&str> {
        self.feature.as_deref()
    }
}

/// 群服务权益判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntitlementDecision {
    /// 允许提供服务。
    Allowed,
    /// 拒绝提供服务；可选附带面向日志或诊断的说明。
    Denied { reason: Option<String> },
}

impl EntitlementDecision {
    /// 构造无附加说明的拒绝决定。
    pub fn denied() -> Self {
        Self::Denied { reason: None }
    }

    /// 构造带说明的拒绝决定。
    pub fn denied_with_reason(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: Some(reason.into()),
        }
    }

    /// 判定是否放行。
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

#[async_trait::async_trait]
pub trait ServiceEntitlement: Send + Sync {
    /// 查询群服务权益状态。错误、超时和提供者异常均拒绝服务；不在此执行履约。
    async fn check_group(
        &self,
        query: &GroupEntitlementQuery,
    ) -> nuono_error::Result<EntitlementDecision>;

    fn requires_assistant_grant(&self) -> bool {
        false
    }
}

#[derive(Debug, Default)]
pub struct AllActive;
#[async_trait::async_trait]
impl ServiceEntitlement for AllActive {
    async fn check_group(
        &self,
        _query: &GroupEntitlementQuery,
    ) -> nuono_error::Result<EntitlementDecision> {
        Ok(EntitlementDecision::Allowed)
    }
}
static ENTITLEMENT: OnceLock<Arc<dyn ServiceEntitlement>> = OnceLock::new();
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("群服务策略已注册；更换部署级实现需要重启")]
pub struct ServiceEntitlementAlreadyRegistered;
pub fn set_service_entitlement(
    entitlement: impl ServiceEntitlement + 'static,
) -> Result<(), ServiceEntitlementAlreadyRegistered> {
    register_with(&ENTITLEMENT, Arc::new(entitlement))
}
fn register_with(
    slot: &OnceLock<Arc<dyn ServiceEntitlement>>,
    policy: Arc<dyn ServiceEntitlement>,
) -> Result<(), ServiceEntitlementAlreadyRegistered> {
    slot.set(policy)
        .map_err(|_| ServiceEntitlementAlreadyRegistered)
}

/// 是否已注册任意部署级或插件级群服务权益策略提供者。
///
/// 在纯开源模式（返回 `false`）下，所有群默认放行，列表接口可跳过逐群异步检查。
pub fn has_entitlement_provider() -> bool {
    ENTITLEMENT.get().is_some() || crate::plugin_extensions::has_entitlement_provider()
}

/// 查询群服务完整权益决定。未安装任何策略提供者时默认放行（开源模式）。
pub async fn check_group_entitlement(query: &GroupEntitlementQuery) -> EntitlementDecision {
    let static_decision = check_with(&ENTITLEMENT, query).await;
    if !static_decision.is_allowed() {
        return static_decision;
    }
    crate::plugin_extensions::check_group_allowed(query).await
}

pub async fn is_group_active(group_id: i64) -> bool {
    check_group_entitlement(&GroupEntitlementQuery::for_group(group_id))
        .await
        .is_allowed()
}
pub fn requires_assistant_grant() -> bool {
    ENTITLEMENT.get().is_some_and(|p| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            p.requires_assistant_grant()
        }))
        .unwrap_or(true)
    }) || crate::plugin_extensions::assistant_grant_required()
}
async fn check_with(
    slot: &OnceLock<Arc<dyn ServiceEntitlement>>,
    query: &GroupEntitlementQuery,
) -> EntitlementDecision {
    match slot.get() {
        None => EntitlementDecision::Allowed,
        Some(provider) => query_provider(provider.clone(), query.clone()).await,
    }
}
#[cfg(test)]
async fn active_with(slot: &OnceLock<Arc<dyn ServiceEntitlement>>, group_id: i64) -> bool {
    check_with(slot, &GroupEntitlementQuery::for_group(group_id))
        .await
        .is_allowed()
}
/// 在独立后台任务中调用策略提供者以隔离潜在的 panic，并在超时（2 秒）时中止任务并默认拒绝放行。
pub(crate) async fn query_provider(
    provider: Arc<dyn ServiceEntitlement>,
    query: GroupEntitlementQuery,
) -> EntitlementDecision {
    struct Query(tokio::task::JoinHandle<nuono_error::Result<EntitlementDecision>>);
    impl Drop for Query {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut task = Query(tokio::spawn(
        async move { provider.check_group(&query).await },
    ));
    match tokio::time::timeout(std::time::Duration::from_secs(2), &mut task.0).await {
        Ok(Ok(Ok(decision))) => decision,
        Ok(_) | Err(_) => EntitlementDecision::denied(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn default_open_and_registered_target_policy_is_stable() {
        struct OnlySeven;
        #[async_trait::async_trait]
        impl ServiceEntitlement for OnlySeven {
            async fn check_group(
                &self,
                query: &GroupEntitlementQuery,
            ) -> nuono_error::Result<EntitlementDecision> {
                if query.group_id() == 7 {
                    Ok(EntitlementDecision::Allowed)
                } else {
                    Ok(EntitlementDecision::denied_with_reason("unpaid"))
                }
            }
        }
        let slot = OnceLock::new();
        assert!(active_with(&slot, 8).await);
        register_with(&slot, Arc::new(OnlySeven)).unwrap();
        assert!(active_with(&slot, 7).await);
        assert!(!active_with(&slot, 8).await);
        assert_eq!(
            register_with(&slot, Arc::new(AllActive)),
            Err(ServiceEntitlementAlreadyRegistered)
        );
        assert!(!active_with(&slot, 8).await);
    }
    #[tokio::test]
    async fn stalled_provider_denies_instead_of_free_service() {
        struct Stalled;
        #[async_trait::async_trait]
        impl ServiceEntitlement for Stalled {
            async fn check_group(
                &self,
                _: &GroupEntitlementQuery,
            ) -> nuono_error::Result<EntitlementDecision> {
                std::future::pending().await
            }
        }
        assert!(
            !query_provider(Arc::new(Stalled), GroupEntitlementQuery::for_group(7))
                .await
                .is_allowed()
        );
    }
}
