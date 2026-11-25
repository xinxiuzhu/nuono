use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, utoipa::ToSchema)]
#[sea_orm(table_name = "nuono_plugin_overrides")]
pub struct Model {
    #[sea_orm(primary_key)]
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub plugin_id: String,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub user_id: Option<i64>,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    #[schema(value_type = Option<String>)]
    pub group_id: Option<i64>,
    pub enabled: Option<bool>,
    pub service_patch_json: String,
    pub config_patch_json: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub revision: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub created_at_ms: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub updated_at_ms: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
