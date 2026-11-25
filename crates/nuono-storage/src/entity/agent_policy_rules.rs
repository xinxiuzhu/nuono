//! Agent 策略规则表实体（`nuono_agent_policy_rules`）。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_agent_policy_rules")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub effect: String,
    pub target_type: String,
    pub target_value: String,
    pub priority: i32,
    pub enabled: bool,
    pub reason: Option<String>,
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
    pub created_at: crate::clock::UtcMillis,
    pub updated_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
