use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_bot_instances")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub platform: String,
    pub namespace: String,
    pub adapter_type: String,
    pub native_bot_id: Option<String>,
    pub enabled: bool,
    pub config_json: String,
    pub runtime_status: String,
    pub last_error: Option<String>,
    pub last_connected_at_ms: Option<i64>,
    pub revision: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
