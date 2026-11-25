//! 群组/会话身份与 AI 角色卡的绑定关系表实体（每个平台会话目标最多绑定一张角色卡）。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_ai_persona_bindings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub target_key: String,
    pub platform: String,
    pub bot_id: String,
    pub group_id: String,
    pub namespace: Option<String>,
    pub persona_id: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
