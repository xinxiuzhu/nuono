use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_ai_models")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub name: String,
    pub provider: String,
    pub provider_type: String,
    pub model_id: String,
    pub api_key: Option<String>,
    pub base_url: String,
    pub is_default: bool,
    pub enabled: bool,
    pub created_at: Option<crate::clock::UtcMillis>,
    pub updated_at: Option<crate::clock::UtcMillis>,
}

pub fn provider_type_display(pt: &str) -> &str {
    match pt {
        "openai_compatible" => "OpenAI 兼容",
        "anthropic" => "Anthropic",
        "gemini" => "Google Gemini",
        _ => pt,
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
