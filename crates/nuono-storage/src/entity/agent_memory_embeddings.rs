//! Agent 记忆向量嵌入缓存表实体（可随时根据权威的记忆表记录重新生成，随关联记忆级联删除）。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_agent_memory_embeddings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub memory_id: i64,
    #[sea_orm(primary_key, auto_increment = false)]
    pub model_id: i64,
    pub model_name: String,
    pub model_revision: String,
    pub content_fingerprint: String,
    pub dimensions: i32,
    pub vector_json: String,
    pub updated_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::agent_memories::Entity",
        from = "Column::MemoryId",
        to = "super::agent_memories::Column::Id",
        on_delete = "Cascade"
    )]
    Memory,
}

impl Related<super::agent_memories::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Memory.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
