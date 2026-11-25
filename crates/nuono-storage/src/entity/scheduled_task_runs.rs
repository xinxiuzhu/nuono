use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, utoipa::ToSchema)]
#[sea_orm(table_name = "nuono_scheduled_task_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub task_id: i64,
    pub task_name: String,
    #[serde(skip_serializing)]
    pub definition_digest: String,
    pub trigger_kind: String,
    pub status: String,
    pub detail: Option<String>,
    pub created_at: crate::clock::UtcMillis,
    pub finished_at: Option<crate::clock::UtcMillis>,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
