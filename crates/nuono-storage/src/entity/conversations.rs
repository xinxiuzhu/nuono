use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_conversations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub platform: String,
    pub bot_instance_id: Option<String>,
    pub scope_type: String,
    pub group_id: Option<i64>,
    pub owner_user_id: Option<i64>,
    pub native_chat_id: String,
    pub native_thread_id: String,
    pub title: Option<String>,
    pub status: String,
    pub summary_text: Option<String>,
    pub last_message_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
