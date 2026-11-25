//! Agent 工具执行回执表实体（`nuono_agent_tool_receipts`）。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_agent_tool_receipts")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
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
    pub input_json: Option<String>,
    pub output_json: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: i32,
    pub created_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
