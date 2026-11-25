//! 插件侧使用的持久化平台身份快照与规范群对象。
//!
//! 任意已解析的外部平台身份在数据库层均强制绑定唯一的内部 `user_id`（占位账号或正式账号），
//! 不再存在无 `user_id` 的悬空访客状态。
use nuono_domain::{ConversationRef, IdentityRef};
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

/// 服务端存储的平台身份与账号状态版本快照（用于事务一致性核验，不是可由客户端伪造的令牌）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityAccess {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub identity_id: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub revision: i64,
    pub identity: IdentityRef,
    pub account: IdentityAccountState,
    /// 当前账号已验证绑定的全部平台身份列表；占位账号状态下仅包含当前平台原生身份自身。
    pub bound_identities: Vec<IdentityRef>,
    /// 完整绑定集合与各身份版本号的哈希摘要；任何绑定、转正、解绑或合并操作都会使该快照失效。
    pub bindings_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityAccountState {
    /// 交互懒创建的占位账号：已具备稳定 `user_id`，但不能登录 Web 或行使管理员权限。
    Shadow {
        #[serde(with = "nuono_domain::scalar::i64_string")]
        user_id: i64,
        status: String,
        #[serde(with = "nuono_domain::scalar::i64_string")]
        auth_epoch: i64,
    },
    /// 已完成网站注册或原地转正的正式账号。
    Account {
        #[serde(with = "nuono_domain::scalar::i64_string")]
        user_id: i64,
        status: String,
        #[serde(with = "nuono_domain::scalar::i64_string")]
        auth_epoch: i64,
    },
}

impl IdentityAccess {
    /// 当前平台身份对应的内部用户主键（占位账号与正式账号均恒为非空正整数）。
    pub fn user_id(&self) -> i64 {
        match &self.account {
            IdentityAccountState::Shadow { user_id, .. }
            | IdentityAccountState::Account { user_id, .. } => *user_id,
        }
    }

    /// 是否已归属正式注册账号。
    pub fn is_registered(&self) -> bool {
        matches!(self.account, IdentityAccountState::Account { .. })
    }

    /// 获取当前活跃的内部用户 ID（占位账号与正式账号均放行）；若账号已被停用、注销或合并，直接拒绝。
    pub fn active_user_id(&self) -> Result<i64> {
        match &self.account {
            IdentityAccountState::Shadow {
                user_id, status, ..
            }
            | IdentityAccountState::Account {
                user_id, status, ..
            } if status == "active" => Ok(*user_id),
            _ => Err(NuonoError::PermissionDenied("关联账号当前不可用".into())),
        }
    }

    /// 获取当前活跃的正式注册账号 ID；占位账号返回 `Ok(None)`，停用/合并账号直接拒绝。
    pub fn active_registered_user_id(&self) -> Result<Option<i64>> {
        match &self.account {
            IdentityAccountState::Account {
                user_id, status, ..
            } if status == "active" => Ok(Some(*user_id)),
            IdentityAccountState::Shadow { status, .. } if status == "active" => Ok(None),
            _ => Err(NuonoError::PermissionDenied("绑定的账号当前不可用".into())),
        }
    }
}

/// 规范化群对象快照（与具体通过哪个机器人实例收发消息无关）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalGroup {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub id: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub revision: i64,
    pub enabled: bool,
    pub identity: ConversationRef,
}
