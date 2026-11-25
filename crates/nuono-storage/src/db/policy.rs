//! 按请求显式传入的插件表浏览与编辑策略校验（独立于数据库存储内容与核心表内置策略）。
use super::{ColumnInspection, TableOwner, inspection::table_owner, management::TableManagement};
pub use nuono_domain::database::{DatabaseColumnPolicy, DatabaseFieldType, DatabaseTablePolicy};

pub(super) fn check_policy(
    table: &str,
    policy: Option<&DatabaseTablePolicy>,
) -> crate::error::Result<()> {
    if let Some(policy) = policy {
        if table_owner(table) == TableOwner::Core || policy.table != table {
            return Err(crate::error::NuonoError::PermissionDenied(
                "插件声明不能覆盖 Core 表或其他表".into(),
            ));
        }
        policy
            .validate()
            .map_err(crate::error::NuonoError::InvalidInput)?;
    }
    Ok(())
}

pub(super) fn management(
    table: &str,
    columns: &[ColumnInspection],
    policy: Option<&DatabaseTablePolicy>,
) -> TableManagement {
    match policy {
        None => TableManagement::for_schema(table, columns),
        Some(policy) => TableManagement {
            create: policy.create,
            update: policy.update,
            delete: policy.delete,
        }
        .with_schema(columns),
    }
}

pub(super) fn compatible(kind: DatabaseFieldType, physical: &str, sqlite: bool) -> bool {
    let physical = physical.to_ascii_lowercase();
    match kind {
        DatabaseFieldType::Text => physical.contains("text") || physical.contains("char"),
        DatabaseFieldType::Boolean => physical.contains("bool") || sqlite && physical == "integer",
        DatabaseFieldType::Integer => {
            matches!(physical.as_str(), "smallint" | "integer" | "bigint")
        }
        DatabaseFieldType::Json => {
            matches!(physical.as_str(), "json" | "jsonb") || sqlite && physical.contains("text")
        }
        _ => false,
    }
}
