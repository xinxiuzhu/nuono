//! Agent 长期记忆表实体（`nuono_agent_memories`）。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_agent_memories")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub scope_type: String,
    pub scope_key: String,
    pub memory_key: String,
    pub content: String,
    pub tags_json: Option<String>,
    /// 是否置顶：为 `None` 时在首次显式置顶/取消置顶或首次被召回前，默认继承标签（`tags_json`）中的 `"pinned"` 状态。
    pub pinned: Option<bool>,
    pub recall_count: i64,
    pub last_recalled_at: Option<crate::clock::UtcMillis>,
    pub enabled: bool,
    pub expires_at: Option<crate::clock::UtcMillis>,
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
    pub created_at: crate::clock::UtcMillis,
    pub updated_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
