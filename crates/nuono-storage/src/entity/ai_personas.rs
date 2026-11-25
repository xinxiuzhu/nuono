//! AI 角色卡（含公共角色卡与个人专属陪伴角色卡）定义表实体；头像、立绘与参考音频等媒体文件按稳定 ID 独立存储在文件系统中。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_ai_personas")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub owner_user_id: Option<i64>,
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub user_address: String,
    pub enabled: bool,
    pub avatar_filename: Option<String>,
    pub portrait_filename: Option<String>,
    pub reference_audio_filename: Option<String>,
    pub reference_text: String,
    pub language: String,
    pub tts_provider: Option<String>,
    pub voice_id: Option<String>,
    pub revision: i64,
    pub updated_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
