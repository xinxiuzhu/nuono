use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_global_configs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    #[sea_orm(column_name = "value_json")]
    pub value: String,
    pub value_version: i64,
    pub revision: i64,
    pub created_at_ms: i64,
    #[sea_orm(column_name = "updated_at_ms")]
    pub updated_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
