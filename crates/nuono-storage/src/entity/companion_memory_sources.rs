//! 陪伴角色长期记忆的对话原文来源与回收状态表实体（独立挂载在每条记忆记录下）。
use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_companion_memory_sources")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub memory_id: i64,
    #[sea_orm(primary_key, auto_increment = false)]
    pub source_id: String,
    pub scope_key: String,
    pub detail: String,
    pub original: String,
    pub importance: f64,
    pub observed_at: crate::clock::UtcMillis,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
