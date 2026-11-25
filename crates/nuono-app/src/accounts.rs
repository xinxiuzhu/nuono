//! 账号角色、状态变更与注销规则。存储层负责在单个原子操作中应用校验后的变更并吊销相关会话。
use crate::auth::{CallerIdentity, Capability, Role};
use nuono_error::{NuonoError, Result};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleChange {
    pub user_id: i64,
    pub username: String,
    pub previous: Role,
    pub current: Role,
    pub revoked_sessions: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusChange {
    pub user_id: i64,
    pub username: String,
    pub previous: String,
    pub current: String,
    pub revoked_sessions: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDeletion {
    pub user_id: i64,
    pub username: String,
    pub revoked_sessions: u64,
}
#[derive(Clone, Debug)]
pub enum AccountMutation {
    Role(Role),
    Status(String),
    Delete,
}
pub enum AccountMutationResult {
    Role(RoleChange),
    Status(StatusChange),
    Delete(UserDeletion),
}
#[derive(Debug)]
pub struct AccountState {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub status: String,
}
/// 在存储层持有账号写锁后、执行实际数据变更前调用，用于校验管理操作是否合法。
pub fn validate_mutation(
    actor: i64,
    target: &AccountState,
    mutation: &AccountMutation,
    active_owners: u64,
    _owned_groups: u64,
) -> Result<()> {
    if matches!(target.status.as_str(), "deleted" | "merged") {
        return Err(NuonoError::NotFound("账号已注销".into()));
    }
    let unchanged = match mutation {
        AccountMutation::Role(role) => *role == target.role,
        AccountMutation::Status(status) => *status == target.status,
        AccountMutation::Delete => false,
    };
    if let AccountMutation::Status(status) = mutation {
        if status != "active" && status != "disabled" {
            return Err(NuonoError::ConfigValidation(
                "只能变更到 active 或 disabled".into(),
            ));
        }
        if target.status != "active" && target.status != "disabled" {
            return Err(NuonoError::PermissionValidation(format!(
                "账号 {} 当前状态为 {}，不能在此变更",
                target.username, target.status
            )));
        }
    }
    if unchanged {
        return Ok(());
    }
    if actor == target.id {
        return Err(NuonoError::PermissionDenied(
            "不能修改或删除自己的账号权限与状态".into(),
        ));
    }
    let removes_owner = target.role == Role::Owner
        && target.status == "active"
        && match mutation {
            AccountMutation::Role(role) => *role != Role::Owner,
            AccountMutation::Status(status) => status == "disabled",
            AccountMutation::Delete => true,
        };
    if removes_owner && active_owners <= 1 {
        return Err(NuonoError::PermissionValidation(
            "这是最后一位所有者，请先指派另一位有效所有者".into(),
        ));
    }
    Ok(())
}
#[async_trait::async_trait]
pub trait AccountStore: Send + Sync {
    async fn mutate_account(
        &self,
        actor_id: i64,
        target_id: i64,
        mutation: AccountMutation,
    ) -> Result<AccountMutationResult>;
    async fn mutate_account_authorized(
        &self,
        caller: &CallerIdentity,
        target_id: i64,
        mutation: AccountMutation,
    ) -> Result<AccountMutationResult>;
}
pub struct AccountService<'a> {
    store: &'a dyn AccountStore,
}
impl<'a> AccountService<'a> {
    pub fn new(store: &'a dyn AccountStore) -> Self {
        Self { store }
    }
    pub async fn mutate(
        &self,
        caller: &CallerIdentity,
        target: i64,
        mutation: AccountMutation,
    ) -> Result<AccountMutationResult> {
        let capability = Capability::AssignRoles;
        if !caller.has(capability) {
            return Err(NuonoError::PermissionDenied(capability.as_str().into()));
        }
        self.store
            .mutate_account_authorized(caller, target, mutation)
            .await
    }
}

impl AccountService<'_> {
    pub async fn set_role(
        &self,
        caller: &CallerIdentity,
        target: i64,
        role: Role,
    ) -> Result<RoleChange> {
        match self
            .mutate(caller, target, AccountMutation::Role(role))
            .await?
        {
            AccountMutationResult::Role(result) => Ok(result),
            _ => unreachable!(),
        }
    }
    pub async fn set_status(
        &self,
        caller: &CallerIdentity,
        target: i64,
        status: &str,
    ) -> Result<StatusChange> {
        match self
            .mutate(caller, target, AccountMutation::Status(status.into()))
            .await?
        {
            AccountMutationResult::Status(result) => Ok(result),
            _ => unreachable!(),
        }
    }
    pub async fn delete(&self, caller: &CallerIdentity, target: i64) -> Result<UserDeletion> {
        match self.mutate(caller, target, AccountMutation::Delete).await? {
            AccountMutationResult::Delete(result) => Ok(result),
            _ => unreachable!(),
        }
    }
}
