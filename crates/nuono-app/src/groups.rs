//! 群对象不归属某一个账号；配置管理权由独立授权来源决定。
use crate::auth::Scope;
use nuono_error::{NuonoError, Result};

/// 当前存在于群内的机器人展示摘要（由同步台账与运行态连接状态合并生成）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct GroupBotPresence {
    pub bot_id: String,
    pub platform: String,
    pub label: String,
    pub online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct GroupRecord {
    /// Nuono 内部群对象 ID，与平台群号分开。
    pub group_id: String,
    pub native_group_id: String,
    pub platform: String,
    /// 平台命名空间（如 QQ 官方机器人与飞书的 `app_id`，OneBot 为空）。
    pub namespace: String,
    /// 来源展示名（如 `OneBot v11`、`QQ 官方 · 小小`），由接口层按实例配置补全。
    pub source_label: String,
    /// 平台是否支持群成员列表与代发群消息（当前为 OneBot 原生群）。
    pub supports_member_management: bool,
    pub group_name: Option<String>,
    pub enabled: Option<bool>,
    /// 该群是否被管理员暂停 AI 自动回复（由运行配置中的 `ai.blocked_groups` 填充）。
    pub ai_blocked: bool,
    pub last_activity: Option<String>,
    /// 当前已知存在于该群的机器人列表。
    #[serde(default)]
    pub bots: Vec<GroupBotPresence>,
}
#[async_trait::async_trait]
pub trait GroupStore: Send + Sync {
    async fn list_groups(&self, user: Option<i64>) -> Result<Vec<GroupRecord>>;
    async fn group_exists(&self, group: i64) -> Result<bool>;
    async fn toggle_group(&self, group: i64, enabled: bool) -> Result<()>;
    async fn rename_group(&self, group: i64, display_name: &str) -> Result<()>;
}
pub struct GroupService<'a> {
    store: &'a dyn GroupStore,
}
impl<'a> GroupService<'a> {
    pub fn new(store: &'a dyn GroupStore) -> Self {
        Self { store }
    }
    pub async fn list(&self, scope: Scope) -> Result<Vec<GroupRecord>> {
        let mut rows = self.store.list_groups(scope.owner_filter()).await?;
        for row in &mut rows {
            let id = row
                .group_id
                .parse::<i64>()
                .map_err(|_| NuonoError::Validation("群对象ID无效".into()))?;
            if scope != Scope::All && !crate::entitlement::is_group_active(id).await {
                row.enabled = None;
                row.last_activity = None;
            }
        }
        Ok(rows)
    }
    /// 平台管理操作属于宿主管理员；配置授权不授予踢人、禁言或代机器人发消息的能力。
    pub async fn ensure_writable(&self, group: i64, scope: Scope) -> Result<()> {
        if scope != Scope::All {
            return Err(NuonoError::PermissionDenied(
                "平台群管理仅限部署管理员；配置授权只用于插件设置".into(),
            ));
        }
        if !self.store.group_exists(group).await? {
            return Err(NuonoError::NotFound("群不存在".into()));
        }
        Ok(())
    }
    pub async fn ensure_serviceable(&self, group: i64, scope: Scope) -> Result<()> {
        if scope == Scope::All || crate::entitlement::is_group_active(group).await {
            return Ok(());
        }
        Err(NuonoError::PermissionDenied("该群当前不可服务".into()))
    }
}
