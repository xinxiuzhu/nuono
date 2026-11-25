//! 应用层账号事务邮件的 SMTP 发送适配器，每次发信按当前配置快照构造。
use super::mail::{self, MailKind};
use crate::{
    config::NuonoConfig,
    db::{Database, REGISTRATION_OTP_TTL_MINUTES},
    error::Result,
};
use nuono_app::account_registration::AccountMail;
use std::sync::Arc;
use tracing::warn;
pub struct AccountMailAdapter {
    db: Database,
    config: Arc<NuonoConfig>,
}
impl AccountMailAdapter {
    pub fn new(db: Database, config: Arc<NuonoConfig>) -> Self {
        Self { db, config }
    }
}
#[async_trait::async_trait]
impl AccountMail for AccountMailAdapter {
    fn ready(&self) -> bool {
        mail::is_ready(&self.config.mail)
    }
    fn registration_enabled(&self) -> bool {
        self.config.mail.registration_enabled
    }
    async fn send_registration_code(&self, request_id: &str, email: &str) -> Result<()> {
        let cfg = &self.config;
        let (_, issued) = self
            .db
            .issue_registration_otp(
                request_id,
                REGISTRATION_OTP_TTL_MINUTES,
                &cfg.security.registration_otp_pepper,
            )
            .await?;
        let Some(issued) = issued else {
            return Ok(());
        };
        let composed = mail::compose_registration_otp(
            &issued.code,
            &cfg.mail.from_name,
            REGISTRATION_OTP_TTL_MINUTES,
        );
        if let Err(error) = mail::send(&cfg.mail, email, &composed).await {
            if let Err(cleanup) = self.db.revoke_registration_otp(&issued.id).await {
                warn!(%cleanup,request_id,"收回未投递的验证码失败");
            }
            let _ = crate::audit::record_event(
                &self.db,
                "account.registration_otp_delivery_failed",
                None,
                None,
                serde_json::json!({"request_id":request_id}),
            )
            .await;
            return Err(error);
        }
        let _ = crate::audit::record_event(
            &self.db,
            "account.registration_otp_sent",
            None,
            None,
            serde_json::json!({"request_id":request_id,"expires_at":issued.expires_at}),
        )
        .await;
        Ok(())
    }
    async fn send_password_reset(&self, user_id: i64, email: &str) -> Result<()> {
        let cfg = &self.config;
        let ttl = cfg.mail.reset_token_ttl_minutes;
        let Some(issued) = self
            .db
            .issue_password_reset_token(user_id, email, ttl)
            .await?
        else {
            return Ok(());
        };
        let composed = mail::compose(
            MailKind::ResetPassword,
            &issued.token,
            &cfg.mail.from_name,
            ttl,
        );
        if let Err(error) = mail::send(&cfg.mail, email, &composed).await {
            // 发信失败时仅吊销本次刚签发的令牌，避免延迟报错误伤并发或稍后重新发送的新邮件令牌。
            if let Err(cleanup) = self.db.revoke_user_token(&issued.token).await {
                warn!(%cleanup,user_id,"收回未投递的邮件令牌失败");
            }
            return Err(error);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl nuono_app::account_provisioning::AccountLinkMail for AccountMailAdapter {
    fn links_ready(&self) -> bool {
        mail::is_ready(&self.config.mail)
    }
    fn email_change_ttl(&self) -> i64 {
        self.config.mail.verify_token_ttl_minutes
    }
    async fn send_email_change_link(&self, email: &str, token: &str, ttl: i64) -> Result<()> {
        let composed = mail::compose(
            MailKind::ConfirmNewEmail,
            token,
            &self.config.mail.from_name,
            ttl,
        );
        if let Err(error) = mail::send(&self.config.mail, email, &composed).await {
            if let Err(cleanup) = self.db.revoke_user_token(token).await {
                warn!(%cleanup,"收回未投递的账号邮件证明失败");
            }
            return Err(error);
        }
        Ok(())
    }
}
