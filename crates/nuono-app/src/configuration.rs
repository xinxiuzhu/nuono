//! 配置写入操作的调用方身份、权限校验器与平台群角色短期凭证。
use nuono_error::{NuonoError, Result};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Debug)]
pub struct ConfigurationActor {
    pub user_id: i64,
    pub mode: ConfigurationMode,
}

#[derive(Clone, Debug)]
pub enum ConfigurationMode {
    /// 后台管理员模式（存储层在事务内仍需回查数据库确认当前操作人仍持有管理员角色）。
    Administrator,
    Account,
    VerifiedPlatformRole(Box<PlatformRoleProof>),
    /// 已核验的插件事件身份模式（存储层在写入事务内会再次核对绑定的身份与群快照版本号）。
    VerifiedIdentity {
        access: Box<crate::plugin_identity::IdentityAccess>,
        group: Box<crate::plugin_identity::CanonicalGroup>,
        role: Option<Box<PlatformRoleProof>>,
    },
}

impl ConfigurationActor {
    pub fn from_caller(caller: &crate::auth::CallerIdentity) -> Self {
        Self {
            user_id: caller.user_id,
            mode: if caller.role.is_staff() {
                ConfigurationMode::Administrator
            } else {
                ConfigurationMode::Account
            },
        }
    }
}

/// 平台群角色核验的目标对象（保留平台原生字符串标识，不降级转换为数字 ID 或回退命名空间）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformRoleSubject {
    pub identity_id: i64,
    pub identity_revision: i64,
    pub group_id: i64,
    pub platform: String,
    pub namespace: String,
    pub native_user_id: String,
    pub conversation_kind: String,
    pub native_parent_id: String,
    pub native_group_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedGroupRole {
    Owner,
    Administrator,
    Member,
}

/// 平台群成员角色查询接口（仅由受信宿主适配器实现，绝不从外部 HTTP 请求参数构造）。
#[async_trait::async_trait]
pub trait PlatformRoleVerifier: Send + Sync {
    fn provider_id(&self) -> &str;
    async fn role(&self, subject: &PlatformRoleSubject) -> Result<VerifiedGroupRole>;
}

#[derive(Debug, Default)]
struct AuthorityState {
    epoch: AtomicU64,
    enabled: RwLock<bool>,
}

/// 宿主维护的平台群角色配置权限校验器。一旦停用该策略或观测到群角色变更，之前签发的所有短期凭证立即失效。
#[derive(Clone, Debug, Default)]
pub struct PlatformRoleAuthority {
    state: Arc<AuthorityState>,
}

impl PlatformRoleAuthority {
    pub fn set_enabled(&self, enabled: bool) {
        let mut current = self
            .state
            .enabled
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if *current != enabled {
            *current = enabled;
            self.invalidate();
        }
    }

    pub fn invalidate(&self) {
        self.state.epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// 每次签发凭证时均实时向平台查询群成员角色；查询失败或非群主/管理员时一律拒绝签发。
    pub async fn verify(
        &self,
        provider: &dyn PlatformRoleVerifier,
        subject: PlatformRoleSubject,
    ) -> Result<PlatformRoleProof> {
        if !*self.state.enabled.read().unwrap_or_else(|e| e.into_inner()) {
            return Err(NuonoError::PermissionDenied(
                "部署未启用平台群角色管理配置".into(),
            ));
        }
        let epoch = self.state.epoch.load(Ordering::Acquire);
        let checked_at_ms = crate::clock::db_now().timestamp_millis();
        if !matches!(
            provider.role(&subject).await?,
            VerifiedGroupRole::Owner | VerifiedGroupRole::Administrator
        ) {
            return Err(NuonoError::PermissionDenied(
                "当前平台身份不是该群群主或管理员".into(),
            ));
        }
        let proof = PlatformRoleProof {
            state: self.state.clone(),
            epoch,
            subject,
            provider_id: provider.provider_id().to_owned(),
            checked_at_ms,
            expires_at_ms: checked_at_ms.saturating_add(30_000),
        };
        proof.validate_current()?;
        Ok(proof)
    }
}

/// 已核验的平台群管理短期凭证（不支持反序列化伪造，调用方仅作透传；存储层在落盘时还会加锁复核绑定的身份记录）。
#[derive(Clone, Debug)]
pub struct PlatformRoleProof {
    state: Arc<AuthorityState>,
    epoch: u64,
    subject: PlatformRoleSubject,
    provider_id: String,
    checked_at_ms: i64,
    expires_at_ms: i64,
}

impl PlatformRoleProof {
    pub fn identity_id(&self) -> i64 {
        self.subject.identity_id
    }
    pub fn identity_revision(&self) -> i64 {
        self.subject.identity_revision
    }
    pub fn group_id(&self) -> i64 {
        self.subject.group_id
    }
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
    pub fn subject(&self) -> &PlatformRoleSubject {
        &self.subject
    }
    pub fn checked_at_ms(&self) -> i64 {
        self.checked_at_ms
    }
    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
    pub fn validate_current(&self) -> Result<()> {
        if self.state.epoch.load(Ordering::Acquire) != self.epoch
            || !*self.state.enabled.read().unwrap_or_else(|e| e.into_inner())
            || crate::clock::db_now().timestamp_millis() >= self.expires_at_ms
        {
            return Err(NuonoError::PermissionDenied(
                "平台角色证明已失效，请重新核验".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Verifier(VerifiedGroupRole);
    #[async_trait::async_trait]
    impl PlatformRoleVerifier for Verifier {
        fn provider_id(&self) -> &str {
            "trusted-fixture"
        }
        async fn role(&self, _: &PlatformRoleSubject) -> Result<VerifiedGroupRole> {
            Ok(self.0)
        }
    }
    fn subject() -> PlatformRoleSubject {
        PlatformRoleSubject {
            identity_id: 2,
            identity_revision: 4,
            group_id: 3,
            platform: "onebot".into(),
            namespace: "".into(),
            native_user_id: "123".into(),
            conversation_kind: "group:numeric".into(),
            native_parent_id: "".into(),
            native_group_id: "456".into(),
        }
    }
    #[tokio::test]
    async fn disabled_unknown_and_invalidated_roles_never_issue_a_live_proof() {
        let authority = PlatformRoleAuthority::default();
        let owner = Verifier(VerifiedGroupRole::Owner);
        assert!(authority.verify(&owner, subject()).await.is_err());
        authority.set_enabled(true);
        assert!(
            authority
                .verify(&Verifier(VerifiedGroupRole::Member), subject())
                .await
                .is_err()
        );
        let proof = authority.verify(&owner, subject()).await.unwrap();
        assert!(proof.validate_current().is_ok());
        authority.invalidate();
        assert!(proof.validate_current().is_err());
        let proof = authority.verify(&owner, subject()).await.unwrap();
        authority.set_enabled(false);
        assert!(proof.validate_current().is_err());
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct OverridePatch {
    /// 期望的覆盖记录主键 ID；与 `expected_revision` 配合使用，防止旧的编辑请求误匹配到被删除后又重建的记录。
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub expected_id: Option<i64>,
    /// 期望的乐观锁版本号（`0` 表示调用方读取时不存在覆盖记录；更新已有记录时必须传入精确的当前版本号）。
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub expected_revision: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub config_version: i64,
    #[serde(default)]
    pub set_config: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub set_service: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub remove_config: Vec<String>,
    #[serde(default)]
    pub remove_service: Vec<String>,
    /// 为 `None` 时保持启用开关不变；不能与 `inherit_enabled = true` 同时设置。
    pub set_enabled: Option<bool>,
    #[serde(default)]
    pub inherit_enabled: bool,
}

/// 合并后的生效配置快照，同时携带后续执行条件更新所需的乐观锁版本号。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginConfigurationSnapshot {
    pub values: serde_json::Value,
    pub enabled: bool,
    pub can_configure: bool,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    pub override_id: Option<i64>,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub revision: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub config_version: i64,
}
/// 宿主为单个已安装插件绑定的专属配置访问服务（调用方只能读写自身插件的配置，无法越权指定其他插件）。
#[async_trait::async_trait]
pub trait PluginConfigurationAccess: Send + Sync {
    async fn configuration_resource_snapshot(
        &self,
        _key: &str,
    ) -> Result<crate::configuration_resources::ConfigurationResource> {
        Err(NuonoError::NotImplemented("宿主未提供结构配置读取".into()))
    }

    async fn configuration_resource_definition(
        &self,
        _key: &str,
    ) -> Result<crate::configuration_resources::ConfigurationResourceDefinition> {
        Err(NuonoError::NotImplemented("插件未声明结构配置资源".into()))
    }
    async fn configuration_resources_committed(&self) -> Result<()> {
        Ok(())
    }

    async fn verify_group_role(
        &self,
        candidates: GroupConfigurationCandidates,
    ) -> Result<Option<PlatformRoleProof>>;

    /// 供后台定时任务等非用户触发流程只读获取某个群的插件生效配置，无需伪造用户身份。
    async fn group_configuration_for_task(
        &self,
        group: &crate::plugin_identity::CanonicalGroup,
    ) -> Result<PluginConfigurationSnapshot>;

    async fn group_configuration(
        &self,
        actor: &crate::plugin_identity::IdentityAccess,
        group: &crate::plugin_identity::CanonicalGroup,
    ) -> Result<PluginConfigurationSnapshot>;
    async fn save_group_configuration(
        &self,
        actor: &crate::plugin_identity::IdentityAccess,
        group: &crate::plugin_identity::CanonicalGroup,
        patch: OverridePatch,
    ) -> Result<PluginConfigurationSnapshot>;
}

/// 由数据库事务提取的待核验平台角色候选列表，交由宿主适配器向平台核实并签发短期凭证。
#[derive(Clone, Debug)]
pub struct GroupConfigurationCandidates {
    pub platform_roles_enabled: bool,
    pub subjects: Vec<PlatformRoleSubject>,
}
