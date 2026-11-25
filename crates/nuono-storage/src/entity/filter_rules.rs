use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_filter_rules")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub filter_type: String,
    pub pattern: String,
    pub match_mode: String,
    pub target_id: Option<String>,
    pub enabled: bool,
    pub created_at: Option<crate::clock::UtcMillis>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
