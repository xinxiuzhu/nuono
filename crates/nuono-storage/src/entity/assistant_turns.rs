use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_assistant_turns")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub user_id: i64,
    pub conversation_id: Option<String>,
    pub card_id: String,
    pub scope_key: String,
    pub context_epoch: i64,
    pub prompt_snapshot: String,
    pub redacted: bool,
    pub control: bool,
    pub compacted: bool,
    pub request_key: String,
    pub text: String,
    pub retry_of: Option<String>,
    pub status: String,
    pub failure: Option<String>,
    pub reply: Option<String>,
    pub quota_day: i64,
    pub delivery: String,
    pub delivery_key: Option<String>,
    pub created_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
