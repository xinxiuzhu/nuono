//! secret（token、API key）的唯一权威存储。
//!
//! 该表与 `global_configs` 分开，便于备份和导出整表排除 secret。值以明文存储，安全边界
//! 依赖数据库文件权限；不要在日志或 API 响应中回显 `value`。
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "nuono_runtime_secrets")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    pub value: Vec<u8>,
    pub owner_plugin_id: Option<String>,
    pub revision: i64,
    pub created_at_ms: i64,
    #[sea_orm(column_name = "updated_at_ms")]
    pub updated_at: crate::clock::UtcMillis,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
