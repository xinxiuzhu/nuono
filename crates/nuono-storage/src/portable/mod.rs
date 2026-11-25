//! 跨数据库可移植备份与离线迁移模块（支持在 SQLite 与 PostgreSQL 之间双向导出、导入和逐行校验强类型数据库快照及配套资产文件）。
//!
//! 执行本模块操作时必须使用专用的离线维护连接：调用方需先停止所有业务写入任务，并全程持有排他部署维护锁，直至数据库与资产文件全部校验完毕。

mod catalog;
pub mod cutover;
pub mod external;
pub mod files;
pub mod purge;
mod versions;
pub use versions::{
    PluginVersions, TARGET_PLUGINS, finish_target_plugin_code, installed_plugin_versions,
    stage_target_plugin_versions, verify_target_plugin_code, verify_target_plugin_storage,
};
mod schema;
mod structure;
mod transfer;

pub use catalog::{RegisteredPlugin, manifest_tables, registered_plugins, registered_schema};
pub use schema::{Column, LogicalType, Migration, Schema, Table};
pub use transfer::{
    Cell, DatabaseImage, DeploymentImage, Sequence, TableImage, export, import, preflight_import,
    set_maintenance, verify, verify_maintenance,
};

#[cfg(test)]
mod acceptance_tests;
#[cfg(test)]
mod registry_tests;
#[cfg(test)]
mod tests;

use crate::error::NuonoError;

/// 跨数据库可移植备份中单个数据库单元格允许持久化的最大字节数（8 MiB）。
pub(crate) const MAX_CELL_BYTES: usize = 8 * 1024 * 1024;

/// 严格解析迁移与恢复相关的 JSON 文档（不会静默丢弃重复键或损失 64 位整数精度）。
///
/// # Errors
/// 若输入不是合法 UTF-8、包含重复 JSON 键、含有 NUL 字符或包含超出可移植范围的整数字面量，则返回错误。
pub fn parse_document<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> crate::error::Result<T> {
    let text = std::str::from_utf8(bytes).map_err(|e| invalid(e.to_string()))?;
    let value = nuono_domain::scalar::parse_json(text)?;
    Ok(serde_json::from_value(value)?)
}

fn invalid(message: impl Into<String>) -> NuonoError {
    NuonoError::DatabaseQuery(format!("逻辑迁移拒绝：{}", message.into()))
}

fn quoted(name: &str) -> crate::error::Result<String> {
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        || !name.as_bytes()[0].is_ascii_lowercase()
    {
        return Err(invalid("表或字段标识不符合便携命名规范"));
    }
    Ok(format!("\"{name}\""))
}

/// 检查数据目录中是否存在尚未完成的离线迁移或插件清理操作日志（即使进程在创建显式 `.pending` 标记文件前中断也能识别）。
///
/// # Errors
/// 若维护日志损坏或体积超限，直接报错阻断宿主启动，绝不静默忽略。
pub fn pending_operation(data: &std::path::Path) -> crate::error::Result<bool> {
    for (name, field, complete) in [
        (
            ".portable-transfer-journal.json",
            "phase",
            serde_json::Value::String("complete".into()),
        ),
        (purge::JOURNAL, "complete", serde_json::Value::Bool(true)),
    ] {
        let path = data.join(name);
        if path.exists() {
            let document: serde_json::Value =
                parse_document(&files::read_bounded(&path, 4 * 1024 * 1024)?)?;
            if document.get(field) != Some(&complete) {
                return Ok(true);
            }
        }
        if path.with_extension("next").exists() {
            return Ok(true);
        }
    }
    Ok(false)
}
