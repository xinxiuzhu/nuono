use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_plugins")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub plugin_id: String,
    pub plugin_role: String,
    pub enabled: bool,
    pub allow_user_overrides: bool,
    pub allow_group_overrides: bool,
    /// 宿主管理的慢调用阈值（毫秒）；None 使用默认值，0 关闭慢调用提示。
    pub slow_threshold_ms: Option<i64>,
    /// 宿主接入策略 JSON；NULL 选择宿主默认值而不重写该行。
    pub trigger_policy_json: Option<String>,
    pub service_config_json: String,
    pub config_json: String,
    pub config_version: i64,
    pub table_prefix: Option<String>,
    pub schema_version: Option<i64>,
    pub code_version: Option<String>,
    pub installed: bool,
    pub portable_manifest_json: Option<String>,
    pub registry_updated_at_ms: Option<i64>,
    pub revision: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
