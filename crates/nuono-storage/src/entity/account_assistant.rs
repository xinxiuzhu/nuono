use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_account_assistant")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub user_id: i64,
    pub active_card_id: Option<String>,
    pub grant_until: Option<crate::clock::UtcMillis>,
    pub grant_daily_limit: Option<i32>,
    pub usage_day: i64,
    pub request_ids: String,
    pub context_epochs_json: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
