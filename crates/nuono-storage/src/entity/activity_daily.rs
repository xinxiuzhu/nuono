use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_activity_daily")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub bucket_key: String,
    pub day: String,
    pub platform: String,
    pub bot_id: String,
    pub user_key: String,
    pub user_id: String,
    pub namespace: String,
    pub scope_key: String,
    pub scope_id: String,
    pub scene: String,
    pub plugin: String,
    pub kind: String,
    pub count: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
