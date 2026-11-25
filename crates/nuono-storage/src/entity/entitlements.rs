//! 统一运行时功能权益实体（覆盖群级与个人级授权，支持计费、人工和试用多来源并存）。

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_entitlements")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub subject_type: String,
    pub subject_id: i64,
    pub feature_key: String,
    pub source: String,
    pub source_ref: Option<String>,
    pub status: String,
    pub starts_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub limits_json: String,
    pub updated_at_ms: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
