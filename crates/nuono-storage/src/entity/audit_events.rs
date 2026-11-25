use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_audit_events")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub category: String,
    pub occurred_at_ms: i64,
    pub actor_user_id: Option<i64>,
    pub actor_plugin_id: Option<String>,
    pub actor_label: Option<String>,
    pub bot_id: Option<String>,
    pub action: String,
    pub result: String,
    pub subject_user_id: Option<i64>,
    pub group_id: Option<i64>,
    pub operation_id: Option<String>,
    pub details_json: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
