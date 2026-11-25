use chrono::{DateTime, Utc};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

pub const MAX_MESSAGE_BYTES: usize = 4096;
pub const MAX_DAILY_LIMIT: u32 = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(default)]
pub struct AssistantSettings {
    pub enabled: bool,
    pub require_grant: bool,
    /// 是否允许普通用户在网页对话中自建、编辑和切换私有角色卡（管理员始终不受限）。
    pub allow_user_custom_cards: bool,
    pub daily_limit: u32,
    /// 原始完整对话保留的天数（超过此天数后逐步压缩为摘要）。
    pub raw_retention_days: u16,
    pub qq_bot_id: Option<String>,
    pub qq_instance_id: Option<String>,
}

impl Default for AssistantSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            require_grant: false,
            allow_user_custom_cards: false,
            daily_limit: 30,
            raw_retention_days: 7,
            qq_bot_id: None,
            qq_instance_id: None,
        }
    }
}

impl AssistantSettings {
    pub fn validate(&self) -> Result<()> {
        if !(1..=365).contains(&self.raw_retention_days) {
            return Err(NuonoError::Validation(
                "完整聊天保留天数应在 1–365 之间".into(),
            ));
        }
        if !(1..=MAX_DAILY_LIMIT).contains(&self.daily_limit) {
            return Err(NuonoError::Validation(
                "助手每日额度须为 1 至 500 条".into(),
            ));
        }
        if self.qq_bot_id.as_deref().is_some_and(|id| !valid_qq(id))
            || self.qq_instance_id.as_deref().is_some_and(|id| {
                self.qq_bot_id.is_none()
                    || id.trim() != id
                    || nuono_domain::NativeId::new(id).is_err()
            })
        {
            return Err(NuonoError::Validation(
                "助手的 QQ 机器人账号或实例 ID 无效".into(),
            ));
        }
        Ok(())
    }

    pub fn qq_route(&self) -> Option<(&str, &str)> {
        let bot = self.qq_bot_id.as_deref()?;
        Some((bot, self.qq_instance_id.as_deref().unwrap_or(bot)))
    }

    pub fn effective_limit(&self, grant: Option<(DateTime<Utc>, u32)>, now: DateTime<Utc>) -> u32 {
        if !self.enabled || !crate::plugin_extensions::assistant_available() {
            return 0;
        }
        if self.require_grant || crate::entitlement::requires_assistant_grant() {
            return grant
                .filter(|(until, _)| *until > now)
                .map_or(0, |(_, limit)| limit);
        }
        self.daily_limit
    }
}

pub fn valid_qq(id: &str) -> bool {
    (5..=19).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_digit())
        && id.parse::<i64>().is_ok_and(|id| id > 0)
        && !id.starts_with('0')
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssistantAdmission {
    Accepted,
    Duplicate,
    Unavailable,
    Exhausted,
}

/// 持久化对话状态；失败及中断均已返还本次预扣额度。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

/// 面向用户的有限失败原因，不包含模型供应商或凭据细节。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailure {
    ModelFailed,
    TimedOut,
    Interrupted,
    Unavailable,
}

impl TurnFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::ModelFailed => {
                "模型暂时无法回复，本次额度已返还。可点击重试，或在 QQ 发送“重试”。"
            }
            Self::TimedOut => "本次回复超时，额度已返还。可点击重试，或在 QQ 发送“重试”。",
            Self::Interrupted => {
                "本次对话因服务中断而结束，额度已返还。可点击重试，或在 QQ 发送“重试”。"
            }
            Self::Unavailable => "助手暂时无法处理这条消息，本次额度已返还。请稍后重试。",
        }
    }
}

/// QQ 投递结果与模型执行结果分别记录；未知投递不自动重放。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnDelivery {
    None,
    Pending,
    Sent,
    Unknown,
}

/// 当前账号可见的对话回执，刷新或重启后仍可查询和显式重试。
#[derive(Clone, Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AssistantTurn {
    pub control: bool,
    pub compacted: bool,
    pub card_id: String,
    pub scope_key: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub context_epoch: i64,
    #[serde(skip)]
    pub prompt_snapshot: String,
    pub id: String,
    pub text: String,
    pub status: TurnStatus,
    pub failure: Option<TurnFailure>,
    pub reply: Option<String>,
    pub delivery: TurnDelivery,
    pub retryable: bool,
    pub retry_of: Option<String>,
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    #[schema(value_type = String)]
    pub created_at: DateTime<Utc>,
}

/// 将框架对话回执映射为确定的执行编号，供重启时核对已生成的回复。
pub fn assistant_run_id(account_id: i64, turn_id: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(format!("assistant:{account_id}:{turn_id}")))
}

/// 一个账号的个人助手访问期限与每日额度。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AssistantGrant {
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    #[schema(value_type = String)]
    pub expires_at: DateTime<Utc>,
    pub daily_limit: u32,
}

impl AssistantGrant {
    /// 校验额度边界。已过期的授权仍可读取以支持续期与撤销。
    pub fn validate(&self) -> Result<()> {
        if !(1..=MAX_DAILY_LIMIT).contains(&self.daily_limit) {
            return Err(NuonoError::Validation(
                "助手每日额度须为 1 至 500 条".into(),
            ));
        }
        Ok(())
    }
}
