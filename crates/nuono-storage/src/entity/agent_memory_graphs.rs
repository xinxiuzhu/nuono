//! 单一记忆作用域下的可重建记忆逻辑关系图谱缓存表实体（`nuono_agent_memory_graphs`）。
use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_agent_memory_graphs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_type: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_key: String,
    pub source_fingerprint: String,
    pub source_limit: i32,
    pub graph_json: String,
    pub updated_at: crate::clock::UtcMillis,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
