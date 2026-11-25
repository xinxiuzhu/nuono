//! 伴侣个人数据清理与生命周期指令记录（仅包含目标作用域标识与截止时间，绝不存对话正文）。
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeletionKind {
    Conversation,
    Forget { memory_ids: Vec<i64> },
    All,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompanionDeletion {
    pub id: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub user_id: i64,
    pub scope_keys: Vec<String>,
    #[serde(with = "nuono_domain::scalar::utc_millis")]
    pub through: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: DeletionKind,
}

impl CompanionDeletion {
    /// 生成单调递增的微秒级上下文世代屏障（epoch），防止数据库回滚或恢复旧备份后重新暴露已要求遗忘的上下文。
    pub fn epoch(&self) -> i64 {
        self.through.timestamp_micros()
    }
    pub fn validate(&self) -> nuono_error::Result<()> {
        let prefix = crate::agent::assistant_chat_key(self.user_id);
        if uuid::Uuid::parse_str(&self.id).is_err()
            || self.epoch() <= 0
            || self.scope_keys.is_empty()
            || self.scope_keys.len() > 33
            || self.scope_keys.iter().any(|key| {
                key != &prefix
                    && key
                        .strip_prefix(&format!("{prefix}:card:"))
                        .is_none_or(|id| uuid::Uuid::parse_str(id).is_err())
            })
            || matches!(&self.kind, DeletionKind::Forget {memory_ids} if memory_ids.len() > 4096 || memory_ids.iter().any(|id| *id <= 0))
        {
            return Err(nuono_error::NuonoError::Validation(
                "个人数据清理记录无效".into(),
            ));
        }
        Ok(())
    }
}
