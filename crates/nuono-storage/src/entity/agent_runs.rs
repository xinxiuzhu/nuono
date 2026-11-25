use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_agent_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: Option<String>,
    pub parent_id: Option<String>,
    pub session_id: String,
    pub status: String,
    pub state_json: String,
    pub approval_id: Option<i64>,
    pub revision: i64,
    pub updated_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
