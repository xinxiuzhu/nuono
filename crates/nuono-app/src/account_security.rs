//! 账号安全凭证（一次性令牌核销、两步验证 MFA）相关业务逻辑。状态变更在持有账号锁的单个事务内一次性落盘。
use crate::auth::{self, CallerIdentity, Capability};
use nuono_error::{NuonoError, Result};

pub enum TokenOperation {
    VerifyEmail,
    ResetPassword { password: String },
    ChangeEmail,
}
impl TokenOperation {
    pub fn purpose(&self) -> &'static str {
        match self {
            Self::VerifyEmail => "email_verification",
            Self::ResetPassword { .. } => "password_reset",
            Self::ChangeEmail => "email_change",
        }
    }
    pub fn password(&self) -> Option<&str> {
        match self {
            Self::ResetPassword { password } => Some(password),
            _ => None,
        }
    }
}
#[derive(Debug)]
pub struct TokenRedemption {
    pub user_id: i64,
    pub revoked_sessions: u64,
}

pub enum MfaOperation {
    Enroll { password: String },
    Confirm { code: String },
    Disable { password: String },
    RegenerateRecovery { password: String, code: String },
}
impl MfaOperation {
    pub fn password(&self) -> Option<&str> {
        match self {
            Self::Enroll { password }
            | Self::Disable { password }
            | Self::RegenerateRecovery { password, .. } => Some(password),
            _ => None,
        }
    }
}
pub struct MfaState {
    pub enabled: bool,
    pub secret: Option<String>,
    pub last_counter: Option<i64>,
}
pub struct MfaPlan {
    pub enabled: bool,
    pub secret: Option<String>,
    pub counter: Option<i64>,
    pub recovery_hashes: Option<Vec<String>>,
    pub result: MfaResult,
}
pub struct MfaResult {
    pub user_id: i64,
    pub username: String,
    pub secret: Option<String>,
    pub otpauth_uri: Option<String>,
    pub recovery_codes: Vec<String>,
}
/// 根据当前加锁读取的账号 MFA 状态计算变更计划（纯函数，不依赖外部 HTTP 层传入的快照）。
pub fn plan_mfa(
    state: MfaState,
    operation: &MfaOperation,
    now: u64,
    username: &str,
) -> Result<MfaPlan> {
    let mut plan = MfaPlan {
        enabled: state.enabled,
        secret: state.secret,
        counter: state.last_counter,
        recovery_hashes: None,
        result: MfaResult {
            user_id: 0,
            username: username.into(),
            secret: None,
            otpauth_uri: None,
            recovery_codes: vec![],
        },
    };
    match operation {
        MfaOperation::Enroll { .. } => {
            if plan.enabled {
                return Err(NuonoError::Conflict(
                    "两步验证已开启，请先关闭再重新绑定".into(),
                ));
            }
            let secret = auth::generate_secret();
            plan.result.otpauth_uri = Some(auth::otpauth_uri("Nuono", username, &secret));
            plan.result.secret = Some(secret.clone());
            plan.secret = Some(secret);
            plan.counter = None;
        }
        MfaOperation::Disable { .. } => {
            if !plan.enabled {
                return Err(NuonoError::Conflict("两步验证未开启".into()));
            }
            plan.enabled = false;
            plan.secret = None;
            plan.counter = None;
        }
        MfaOperation::Confirm { code } | MfaOperation::RegenerateRecovery { code, .. } => {
            match operation {
                MfaOperation::Confirm { .. } if plan.enabled => {
                    return Err(NuonoError::Conflict("两步验证已开启".into()));
                }
                MfaOperation::RegenerateRecovery { .. } if !plan.enabled => {
                    return Err(NuonoError::Conflict("两步验证未开启".into()));
                }
                _ => {}
            }
            let secret = plan
                .secret
                .as_deref()
                .ok_or_else(|| NuonoError::Validation("请先发起开启两步验证".into()))?;
            let step = auth::validate_code_counter(secret, code, now)
                .filter(|step| plan.counter.is_none_or(|old| *step > old as u64))
                .ok_or_else(|| NuonoError::Validation("验证码不正确，请重试".into()))?;
            plan.enabled = true;
            plan.counter = Some(step as i64);
            plan.result.recovery_codes = auth::generate_recovery_codes();
            plan.recovery_hashes = Some(
                plan.result
                    .recovery_codes
                    .iter()
                    .map(|code| auth::recovery_code_hash(code))
                    .collect(),
            );
        }
    }
    Ok(plan)
}
#[async_trait::async_trait]
pub trait AccountSecurityStore: Send + Sync {
    async fn redeem_account_token(
        &self,
        token: &str,
        operation: TokenOperation,
    ) -> Result<Option<TokenRedemption>>;
    async fn change_account_mfa(&self, user_id: i64, operation: MfaOperation) -> Result<MfaResult>;
}
pub struct AccountSecurityService<'a> {
    store: &'a dyn AccountSecurityStore,
}
impl<'a> AccountSecurityService<'a> {
    pub fn new(store: &'a dyn AccountSecurityStore) -> Self {
        Self { store }
    }
    pub async fn redeem(
        &self,
        token: &str,
        operation: TokenOperation,
    ) -> Result<Option<TokenRedemption>> {
        if let Some(password) = operation.password() {
            auth::validate_password(password).map_err(NuonoError::ConfigValidation)?;
        }
        self.store.redeem_account_token(token, operation).await
    }
    pub async fn change_mfa(
        &self,
        caller: &CallerIdentity,
        operation: MfaOperation,
    ) -> Result<MfaResult> {
        if !caller.has(Capability::ManageOwnAccount) {
            return Err(NuonoError::PermissionDenied("manage_own_account".into()));
        }
        self.store
            .change_account_mfa(caller.user_id, operation)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confirmation_and_recovery_regeneration_reject_a_replayed_step() {
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ".to_string();
        let confirmed = plan_mfa(
            MfaState {
                enabled: false,
                secret: Some(secret.clone()),
                last_counter: None,
            },
            &MfaOperation::Confirm {
                code: "287082".into(),
            },
            59,
            "alice",
        )
        .unwrap();
        assert!(confirmed.enabled);
        assert_eq!(confirmed.counter, Some(1));
        assert_eq!(
            confirmed.result.recovery_codes.len(),
            auth::RECOVERY_CODE_COUNT
        );
        assert!(
            plan_mfa(
                MfaState {
                    enabled: true,
                    secret: Some(secret),
                    last_counter: Some(1)
                },
                &MfaOperation::RegenerateRecovery {
                    password: "irrelevant-to-pure-plan".into(),
                    code: "287082".into()
                },
                59,
                "alice"
            )
            .is_err()
        );
    }
}
