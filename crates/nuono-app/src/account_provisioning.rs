//! 账号创建及换绑邮箱业务逻辑。进入此模块前由 HTTP 层完成调用方身份认证。
use crate::auth::{self, CallerIdentity, Capability, Role};
use nuono_error::{NuonoError, Result};

#[derive(Debug, Clone)]
pub struct ManagedAccount {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub role: String,
    pub status: String,
    pub last_login_at: Option<String>,
    pub created_at: String,
}
pub struct NewManagedAccount {
    pub username: String,
    pub email: Option<String>,
    pub password: String,
    pub role: Role,
}
pub struct AccountLink {
    pub account: ManagedAccount,
    pub token: String,
}
#[derive(Debug, thiserror::Error)]
pub enum ProvisioningError {
    #[error("邮件功能尚未就绪，暂时无法发送验证码")]
    Unavailable,
    #[error("{0}")]
    Delivery(String),
    #[error(transparent)]
    Account(#[from] NuonoError),
}
/// 账号开通与换绑邮箱的存储层接口。实现方需在账号管理互斥锁内再次校验操作人权限并执行写入。
#[async_trait::async_trait]
pub trait AccountProvisioningStore: Send + Sync {
    async fn create_managed_account(
        &self,
        actor_id: i64,
        input: NewManagedAccount,
    ) -> Result<ManagedAccount>;
    /// 在持有账号锁的情况下校验当前密码并生成换绑邮箱凭证；若目标邮箱已被占用，对外仍返回一致的成功响应以防邮箱枚举。
    async fn prepare_email_change(
        &self,
        user_id: i64,
        password: &str,
        email: &str,
        ttl: i64,
    ) -> Result<Option<AccountLink>>;
}
#[async_trait::async_trait]
pub trait AccountLinkMail: Send + Sync {
    fn links_ready(&self) -> bool;
    fn email_change_ttl(&self) -> i64;
    async fn send_email_change_link(&self, email: &str, token: &str, ttl: i64) -> Result<()>;
}
pub fn validate_provisioning_role(role: Role, actor_role: Role) -> Result<()> {
    if !actor_role.has(Capability::ManageUsers) {
        return Err(NuonoError::PermissionDenied("manage_users".into()));
    }
    if role == Role::Owner {
        return Err(NuonoError::ConfigValidation(
            "只能选择管理员或普通成员".into(),
        ));
    }
    if role == Role::Admin && !actor_role.has(Capability::AssignRoles) {
        return Err(NuonoError::PermissionDenied(
            "只有所有者能创建管理员账号".into(),
        ));
    }
    Ok(())
}
pub struct AccountProvisioningService<'a> {
    store: &'a dyn AccountProvisioningStore,
}
impl<'a> AccountProvisioningService<'a> {
    pub fn new(store: &'a dyn AccountProvisioningStore) -> Self {
        Self { store }
    }
    fn input(
        caller: &CallerIdentity,
        username: &str,
        email: Option<&str>,
        password: &str,
        role: Option<&str>,
    ) -> Result<NewManagedAccount> {
        let username = username.trim().to_string();
        auth::validate_username(&username).map_err(NuonoError::ConfigValidation)?;
        let email = email
            .map(str::trim)
            .filter(|email| !email.is_empty())
            .map(str::to_ascii_lowercase);
        if let Some(email) = &email {
            auth::validate_email(email).map_err(NuonoError::ConfigValidation)?;
        }
        auth::validate_password(password).map_err(NuonoError::ConfigValidation)?;
        let role = Role::parse(role.unwrap_or("user"))
            .ok_or_else(|| NuonoError::ConfigValidation("只能选择管理员或普通成员".into()))?;
        validate_provisioning_role(role, caller.role)?;
        Ok(NewManagedAccount {
            username,
            email,
            password: password.into(),
            role,
        })
    }
    pub async fn create(
        &self,
        caller: &CallerIdentity,
        username: &str,
        email: Option<&str>,
        password: &str,
        role: Option<&str>,
    ) -> Result<ManagedAccount> {
        self.store
            .create_managed_account(
                caller.user_id,
                Self::input(caller, username, email, password, role)?,
            )
            .await
    }
    pub async fn request_email_change(
        &self,
        caller: &CallerIdentity,
        password: &str,
        email: &str,
        mail: &dyn AccountLinkMail,
    ) -> std::result::Result<(), ProvisioningError> {
        if !caller.has(Capability::ManageOwnAccount) {
            return Err(NuonoError::PermissionDenied("manage_own_account".into()).into());
        }
        if !mail.links_ready() {
            return Err(ProvisioningError::Unavailable);
        }
        let email = email.trim().to_ascii_lowercase();
        auth::validate_email(&email).map_err(NuonoError::ConfigValidation)?;
        let ttl = mail.email_change_ttl();
        if let Some(link) = self
            .store
            .prepare_email_change(caller.user_id, password, &email, ttl)
            .await?
        {
            mail.send_email_change_link(&email, &link.token, ttl)
                .await
                .map_err(|error| {
                    tracing::warn!(%error,"更换邮箱邮件投递失败");
                    ProvisioningError::Delivery("邮件发送失败，请稍后再试".into())
                })?;
        }
        Ok(())
    }
}
