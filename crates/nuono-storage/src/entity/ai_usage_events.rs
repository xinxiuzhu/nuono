//! 模型服务商返回的 AI 用量统计事件表实体；刻意不包含提示词、密钥凭据或模型回复正文。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_ai_usage_events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub request_id: String,
    pub started_at_ms: i64,
    pub day: String,
    pub model_id: Option<i64>,
    pub model: String,
    pub name: String,
    pub provider: String,
    pub source: String,
    pub operation: String,
    pub outcome: String,
    pub elapsed_ms: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
