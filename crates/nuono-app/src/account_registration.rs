//! 用户自助注册与找回密码服务。注册流程需先验证邮箱所有权，验证通过后才会正式分配账号 ID。
use crate::auth;
use nuono_error::{NuonoError, Result};
#[derive(Debug, Clone)]
pub struct RegistrationAccount {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub status: String,
    pub email_verified: bool,
}
#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    #[error("邮件功能尚未就绪，无法自助注册")]
    Unavailable,
    #[error("自助注册已关闭。请联系管理员开通账号。")]
    Disabled,
    #[error("该用户名已被占用")]
    UsernameTaken,
    #[error("该邮箱已注册，请直接登录、找回密码或联系管理员")]
    EmailTaken,
    #[error(transparent)]
    Other(#[from] NuonoError),
}
#[derive(Debug, Clone)]
pub struct RegistrationReceipt {
    pub request_id: String,
    pub created: bool,
}
#[async_trait::async_trait]
pub trait RegistrationStore: Send + Sync {
    async fn registration_by_name(&self, username: &str) -> Result<Option<RegistrationAccount>>;
    async fn registration_by_email(&self, email: &str) -> Result<Option<RegistrationAccount>>;
    async fn create_registration_request(
        &self,
        username: &str,
        email: &str,
        password: &str,
    ) -> Result<RegistrationReceipt>;
    async fn registration_request_for_email(&self, email: &str) -> Result<Option<String>>;
}
#[async_trait::async_trait]
pub trait AccountMail: Send + Sync {
    fn ready(&self) -> bool;
    fn registration_enabled(&self) -> bool;
    async fn send_registration_code(&self, request_id: &str, email: &str) -> Result<()>;
    async fn send_password_reset(&self, user_id: i64, email: &str) -> Result<()>;
}
pub struct RegistrationService<'a> {
    store: &'a dyn RegistrationStore,
    mail: &'a dyn AccountMail,
}
impl<'a> RegistrationService<'a> {
    pub fn new(store: &'a dyn RegistrationStore, mail: &'a dyn AccountMail) -> Self {
        Self { store, mail }
    }
    pub async fn register(
        &self,
        username: &str,
        email: &str,
        password: &str,
    ) -> std::result::Result<RegistrationReceipt, RegistrationError> {
        if !self.mail.ready() {
            return Err(RegistrationError::Unavailable);
        }
        if !self.mail.registration_enabled() {
            return Err(RegistrationError::Disabled);
        }
        let username = username.trim();
        let email = email.trim().to_ascii_lowercase();
        auth::validate_username(username).map_err(NuonoError::ConfigValidation)?;
        auth::validate_email(&email).map_err(NuonoError::ConfigValidation)?;
        auth::validate_password(password).map_err(NuonoError::ConfigValidation)?;
        if self.store.registration_by_name(username).await?.is_some() {
            return Err(RegistrationError::UsernameTaken);
        }
        if self.store.registration_by_email(&email).await?.is_some() {
            return Err(RegistrationError::EmailTaken);
        }
        let receipt = self
            .store
            .create_registration_request(username, &email, password)
            .await?;
        if let Err(error) = self
            .mail
            .send_registration_code(&receipt.request_id, &email)
            .await
        {
            tracing::warn!(%error,request_id=%receipt.request_id,"注册验证码投递失败");
        }
        Ok(receipt)
    }
    pub async fn resend_verification(&self, email: &str) -> Result<()> {
        if !self.mail.ready() || !self.mail.registration_enabled() {
            return Err(NuonoError::Validation("自助注册当前不可用".into()));
        }
        let email = email.trim().to_ascii_lowercase();
        auth::validate_email(&email).map_err(NuonoError::ConfigValidation)?;
        if let Some(id) = self.store.registration_request_for_email(&email).await?
            && let Err(error) = self.mail.send_registration_code(&id, &email).await
        {
            tracing::warn!(%error,"验证码重发失败");
        }
        Ok(())
    }
    pub async fn forgot_password(&self, email: &str) -> Result<()> {
        if !self.mail.ready() {
            return Err(NuonoError::Validation("邮件功能尚未就绪".into()));
        }
        let email = email.trim().to_ascii_lowercase();
        auth::validate_email(&email).map_err(NuonoError::ConfigValidation)?;
        if let Ok(Some(account)) = self.store.registration_by_email(&email).await
            && account.email_verified
            && account.status == "active"
            && let Err(error) = self.mail.send_password_reset(account.id, &email).await
        {
            tracing::warn!(%error,"密码重置邮件投递失败");
        }
        Ok(())
    }
}
