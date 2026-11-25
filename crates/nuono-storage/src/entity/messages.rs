use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_messages")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub conversation_id: Option<String>,
    pub platform: String,
    pub bot_instance_id: Option<String>,
    pub native_message_id: Option<String>,
    pub reply_to_native_message_id: Option<String>,
    pub sender_user_id: Option<i64>,
    pub sender_identity_id: Option<i64>,
    pub native_sender_id: Option<String>,
    pub native_group_id: Option<String>,
    pub sender_nickname: Option<String>,
    pub direction: String,
    pub role: String,
    pub text_content: String,
    pub content_blocks_json: String,
    pub raw_payload_json: Option<String>,
    pub occurred_at_ms: i64,
    pub created_at_ms: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
