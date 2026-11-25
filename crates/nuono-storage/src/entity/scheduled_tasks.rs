use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_scheduled_tasks")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub name: String,
    pub cron_expr: String,
    pub timezone: String,
    pub revision: i64,
    pub action_type: String,
    pub action_params: Option<String>,
    /// 投递 Bot 的 OneBot self_id；NULL 表示老任务（执行时广播并告警）
    pub bot_id: Option<String>,
    pub enabled: Option<bool>,
    pub last_run_at: Option<crate::clock::UtcMillis>,
    pub created_at: Option<crate::clock::UtcMillis>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
