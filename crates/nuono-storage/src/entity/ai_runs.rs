use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_ai_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: String,
    pub trigger_message_id: Option<i64>,
    pub reply_message_id: Option<i64>,
    pub user_id: Option<i64>,
    pub provider: String,
    pub model: String,
    pub status: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub latency_ms: i32,
    pub steps_json: String,
    pub error_message: Option<String>,
    pub created_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
