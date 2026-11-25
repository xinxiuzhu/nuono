//! Agent 工具调用审批表实体（`nuono_agent_tool_approvals`）。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_agent_tool_approvals")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub run_id: Option<String>,
    pub session_id: String,
    pub context_json: Option<String>,
    pub bot_id: i64,
    pub platform: String,
    pub message_type: String,
    pub user_id: i64,
    pub group_id: Option<i64>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_name: String,
    pub risk_level: String,
    pub status: String,
    pub execution_json: Option<String>,
    pub input_json: Option<String>,
    pub reason: Option<String>,
    pub decision_by: Option<String>,
    pub decision_reason: Option<String>,
    pub decided_at: Option<crate::clock::UtcMillis>,
    pub created_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
