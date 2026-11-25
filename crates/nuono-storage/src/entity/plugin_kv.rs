use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_plugin_kv")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub collection: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub scope_key: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub record_key: String,
    pub generation: String,
    pub expires_at_ms: Option<i64>,
    pub value_json: String,
    pub value_version: i32,
    pub revision: i64,
    pub created_at_ms: i64,
    #[sea_orm(column_name = "updated_at_ms")]
    pub updated_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
