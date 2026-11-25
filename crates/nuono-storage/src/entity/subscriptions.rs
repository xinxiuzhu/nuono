//! 内容与主题推送订阅实体；投递目标由 subscriber_id 和可选 target_id 确定。

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_feed_subscriptions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub subscriber_id: String,
    pub topic: String,
    pub target_id: Option<String>,
    pub enabled: Option<bool>,
    pub created_at: Option<crate::clock::UtcMillis>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
