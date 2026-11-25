//! 插件存储表归属与不可变迁移版本的持久化目录（供跨数据库可移植备份与迁移使用）。

use std::collections::BTreeSet;

use nuono_app::plugin_manifest::{
    PluginStorageDeclaration, PortableManifest, validate_table_prefix,
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};

use super::{Migration, Schema, invalid, quoted};
use crate::{Database, error::Result};

/// 数据库中持久保留的最新插件存储声明（插件数据表的归属由数据库台账决定，与当前可执行文件是否加载了该插件代码无关）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisteredPlugin {
    pub plugin_id: String,
    pub code_version: Option<String>,
    pub installed: bool,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    pub version: Option<i64>,
    pub manifest: PortableManifest,
}

/// 从持久化注册台账中读取所有已安装及已卸载插件的存储归属声明。
///
/// # Errors
/// 若数据库中保留的插件声明格式损坏或与迁移历史不一致，则直接报错拒绝，绝不静默跳过该插件的数据表。
pub async fn registered_plugins<C: ConnectionTrait>(conn: &C) -> Result<Vec<RegisteredPlugin>> {
    let rows = conn.query_all_raw(Statement::from_string(conn.get_database_backend(),
        "SELECT r.plugin_id,r.table_prefix,r.schema_version,r.portable_manifest_json,r.code_version,r.installed,(SELECT MAX(m.version) FROM nuono_plugin_migrations m WHERE m.plugin_id=r.plugin_id) AS applied_version FROM nuono_plugins r WHERE r.table_prefix IS NOT NULL ORDER BY r.plugin_id")).await?;
    let mut plugins = Vec::new();
    let mut owned = BTreeSet::new();
    for row in rows {
        let plugin_id = row.try_get::<String>("", "plugin_id")?;
        let manifest: PortableManifest = super::parse_document(
            row.try_get::<String>("", "portable_manifest_json")?
                .as_bytes(),
        )?;
        let version = row.try_get::<Option<i64>>("", "schema_version")?;
        if manifest.table_prefix != row.try_get::<String>("", "table_prefix")?
            || version != row.try_get::<Option<i64>>("", "applied_version")?
            || (version.is_none() && !manifest_tables(&manifest)?.is_empty())
        {
            return Err(invalid(format!(
                "插件 {plugin_id} 注册信息与迁移历史不一致"
            )));
        }
        for table in manifest_tables(&manifest)? {
            if !owned.insert(table.clone()) {
                return Err(invalid(format!("多个插件声明同一张表 {table}")));
            }
        }
        let installed = if conn.get_database_backend() == DbBackend::Postgres {
            row.try_get::<Option<bool>>("", "installed")?
                .unwrap_or(false)
        } else {
            row.try_get::<Option<i64>>("", "installed")?.unwrap_or(0) == 1
        };
        plugins.push(RegisteredPlugin {
            plugin_id,
            code_version: row.try_get("", "code_version")?,
            installed,
            version,
            manifest,
        });
    }
    Ok(plugins)
}

/// 在显式清理（purge）插件数据后清空其表结构与存储归属声明，但保留该插件的安装记录与表前缀命名空间。
pub(super) async fn clear_storage_registration<C: ConnectionTrait>(
    conn: &C,
    plugin_id: &str,
) -> Result<()> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            conn.get_database_backend(),
            "SELECT table_prefix FROM nuono_plugins WHERE plugin_id=$1 AND table_prefix IS NOT NULL",
            [plugin_id.into()],
        ))
        .await?
        .ok_or_else(|| invalid("插件缺少存储注册信息"))?;
    let mut manifest = PortableManifest::empty(plugin_id);
    manifest.table_prefix = row.try_get("", "table_prefix")?;
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_raw(Statement::from_sql_and_values(conn.get_database_backend(),
        "UPDATE nuono_plugins SET schema_version=NULL,portable_manifest_json=$1,registry_updated_at_ms=$2,updated_at_ms=$2 WHERE plugin_id=$3",
        [serde_json::to_string(&manifest)?.into(),now.into(),plugin_id.into()])).await?;
    Ok(())
}

/// 校验插件跨数据库可移植清单（PostgreSQL 与 SQLite 双端建表 SQL 及账号/身份外键引用声明），并返回该插件归属的精确表名集合。
///
/// # Errors
/// 若试图占用 `nuono_` 核心表前缀、表名超出声明前缀、双端表结构不一致或外键未声明合并/认领策略，则返回错误。
pub fn manifest_tables(manifest: &PortableManifest) -> Result<BTreeSet<String>> {
    validate_table_prefix(&manifest.table_prefix)?;
    if manifest.format != 1 {
        return Err(invalid("不支持的持久清单版本"));
    }
    if manifest.postgres_schema.trim().is_empty() && manifest.sqlite_schema.trim().is_empty() {
        if !manifest.account_references.is_empty() {
            return Err(invalid("空清单不能声明账号业务列"));
        }
        return Ok(BTreeSet::new());
    }
    if manifest.table_prefix.starts_with("nuono_") || !manifest.table_prefix.ends_with('_') {
        return Err(invalid("插件表前缀必须稳定、以 _ 结尾且不能占用 nuono_"));
    }
    quoted(&manifest.table_prefix)?;
    let schema = Schema::new(vec![Migration {
        name: "plugin-manifest".into(),
        postgres_sql: manifest.postgres_schema.clone(),
        sqlite_sql: manifest.sqlite_schema.clone(),
    }])?;
    for table in schema.tables.values() {
        if !table.name.starts_with(&manifest.table_prefix) {
            return Err(invalid(format!("{} 不属于声明前缀", table.name)));
        }
    }
    let index=regex::Regex::new(r#"(?i)^CREATE\s+(?:UNIQUE\s+)?INDEX\s+\"?([a-z][a-z0-9_]*)\"?\s+ON\s+\"?([a-z][a-z0-9_]*)\"?\s*(?:USING\s+[a-z]+\s*)?\("#).map_err(|e|invalid(e.to_string()))?;
    for script in [&manifest.postgres_schema, &manifest.sqlite_schema] {
        let clean = super::schema::strip_line_comments(script);
        for statement in super::schema::split_sql(&clean, ';')? {
            if statement
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("CREATE INDEX")
                || statement
                    .trim_start()
                    .to_ascii_uppercase()
                    .starts_with("CREATE UNIQUE INDEX")
            {
                let capture = index
                    .captures(statement.trim())
                    .ok_or_else(|| invalid("索引声明必须有准确名称及所属表"))?;
                if !capture[1].starts_with(&manifest.table_prefix)
                    || !schema.tables.contains_key(&capture[2])
                {
                    return Err(invalid("插件索引超出本插件表归属"));
                }
            }
        }
    }
    for reference in &manifest.account_references {
        let table = schema
            .tables
            .get(&reference.table)
            .ok_or_else(|| invalid("账号引用不属于插件表"))?;
        if !table
            .columns
            .iter()
            .any(|c| c.name == reference.column && c.kind == super::LogicalType::Integer)
        {
            return Err(invalid("账号引用必须指向声明的整数列"));
        }
    }
    let declared = manifest
        .account_references
        .iter()
        .map(|r| (r.table.clone(), r.column.clone()))
        .collect::<BTreeSet<_>>();
    if declared.len() != manifest.account_references.len() {
        return Err(invalid("同一账号引用不能重复声明合并策略"));
    }
    for sql in [&manifest.postgres_schema, &manifest.sqlite_schema] {
        for reference in core_foreign_keys(sql, "nuono_users")? {
            if !declared.contains(&reference) {
                return Err(invalid(format!(
                    "账号外键 {}.{} 未声明合并策略",
                    reference.0, reference.1
                )));
            }
        }
    }
    Ok(schema.tables.into_keys().collect())
}

fn core_foreign_keys(sql: &str, target: &str) -> Result<BTreeSet<(String, String)>> {
    let cleaned = super::schema::strip_line_comments(sql);
    let table_re = regex::Regex::new(
        r#"(?i)^CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?\"?([a-z][a-z0-9_]*)\"?\s*\("#,
    )
    .map_err(|e| invalid(e.to_string()))?;
    let reference_re = regex::Regex::new(&format!(
        r#"(?i)REFERENCES\s+\"?{}\"?\s*\("#,
        regex::escape(target)
    ))
    .map_err(|e| invalid(e.to_string()))?;
    let foreign_re = regex::Regex::new(r"(?i)FOREIGN\s+KEY\s*\(([^)]+)\)")
        .map_err(|e| invalid(e.to_string()))?;
    let mut result = BTreeSet::new();
    for statement in super::schema::split_sql(&cleaned, ';')? {
        let Some(caps) = table_re.captures(statement.trim()) else {
            continue;
        };
        let whole = caps.get(0).ok_or_else(|| invalid("表声明无效"))?;
        let statement = statement.trim();
        let end = statement
            .rfind(')')
            .ok_or_else(|| invalid("表声明缺少右括号"))?;
        for field in super::schema::split_sql(&statement[whole.end()..end], ',')? {
            if !reference_re.is_match(field) {
                continue;
            }
            let columns = if let Some(foreign) = foreign_re.captures(field) {
                foreign[1]
                    .split(',')
                    .map(|v| v.trim().trim_matches('"').to_owned())
                    .collect::<Vec<_>>()
            } else {
                vec![
                    field
                        .split_whitespace()
                        .next()
                        .ok_or_else(|| invalid("账号外键缺少列"))?
                        .trim_matches('"')
                        .to_owned(),
                ]
            };
            for column in columns {
                quoted(&column)?;
                result.insert((caps[1].into(), column));
            }
        }
    }
    Ok(result)
}

/// 结合框架核心迁移、已启用可选模块及所有已登记插件（含已卸载但仍保留数据的插件）的持久化清单，构建全库完整的可移植表结构定义。
///
/// # Errors
/// 若数据库中的框架迁移版本/校验和与当前程序不匹配，或插件持久化声明存在冲突，则返回错误。
pub async fn registered_schema<C: ConnectionTrait>(conn: &C) -> Result<Schema> {
    let rows = conn
        .query_all_raw(Statement::from_string(
            conn.get_database_backend(),
            "SELECT module,version,description,checksum FROM nuono_schema_migrations ORDER BY module,version",
        ))
        .await?;
    let mut core_rows = Vec::new();
    let mut ai_rows = Vec::new();
    for row in rows {
        match row.try_get::<String>("", "module")?.as_str() {
            "core" => core_rows.push(row),
            "ai" => ai_rows.push(row),
            other => return Err(invalid(format!("框架账本包含未知模块 {other}"))),
        }
    }
    let compiled = crate::db::framework_migration_sources();
    if core_rows.len() != compiled.len() {
        return Err(invalid("框架权威账本版本数量与程序不匹配"));
    }
    for (index, (row, (name, _, _))) in core_rows.iter().zip(compiled).enumerate() {
        let version = usize::try_from(row.try_get::<i64>("", "version")?)
            .map_err(|_| invalid("框架版本无效"))?;
        if version != index + 1
            || row.try_get::<String>("", "description")? != name
            || row.try_get::<String>("", "checksum")?
                != crate::db::migrations::framework_checksum(index)?
        {
            return Err(invalid("框架账本版本、摘要或完整声明与匹配程序不同"));
        }
    }
    let mut modules = Vec::new();
    if !ai_rows.is_empty() {
        let compiled_ai = crate::db::ai_module_migration_sources();
        if ai_rows.len() != compiled_ai.len() {
            return Err(invalid("AI 模块权威账本版本数量与程序不匹配"));
        }
        for (index, (row, (name, _, _))) in ai_rows.iter().zip(compiled_ai).enumerate() {
            let version = usize::try_from(row.try_get::<i64>("", "version")?)
                .map_err(|_| invalid("AI 模块版本无效"))?;
            if version != index + 1
                || row.try_get::<String>("", "description")? != name
                || row.try_get::<String>("", "checksum")?
                    != crate::db::migrations::ai_module_checksum(index)?
            {
                return Err(invalid("AI 模块账本版本、摘要或完整声明与匹配程序不同"));
            }
        }
        modules.push("ai".to_string());
    }
    let mut extensions = Vec::new();
    for plugin in registered_plugins(conn).await? {
        if !manifest_tables(&plugin.manifest)?.is_empty() {
            extensions.push(Migration {
                name: format!(
                    "{}:{}",
                    plugin.plugin_id,
                    plugin
                        .version
                        .ok_or_else(|| invalid("非空业务清单缺少版本"))?
                ),
                postgres_sql: plugin.manifest.postgres_schema,
                sqlite_sql: plugin.manifest.sqlite_schema,
            });
        }
    }
    Schema::framework_with_modules(&modules, extensions)
}

impl Database {
    /// 登记宿主启动时实际引用的外置文件路径清单（仅记录路径与元数据供离线迁移预检使用，不包含任何密钥明文）。
    ///
    /// # Errors
    /// 若资产键重复、外置路径不是绝对路径或数据库写入失败，则返回错误。
    pub async fn register_deployment_assets(
        &self,
        assets: &[nuono_app::plugin_manifest::PersistentAsset],
    ) -> Result<()> {
        let mut keys = BTreeSet::new();
        for asset in assets {
            match asset {
                nuono_app::plugin_manifest::PersistentAsset::External { key, path, .. }
                | nuono_app::plugin_manifest::PersistentAsset::Reference { key, path, .. }
                    if keys.insert(key) && std::path::Path::new(path).is_absolute() => {}
                _ => return Err(invalid("框架外置资产须有唯一键和绝对路径")),
            }
        }
        self.orm()
            .execute_raw(Statement::from_sql_and_values(
                self.backend(),
                "UPDATE nuono_deployment SET assets_json=$1 WHERE id=1",
                [serde_json::to_string(assets)?.into()],
            ))
            .await?;
        Ok(())
    }
    /// 在单个迁移事务内原子执行插件的双端配对建表迁移，并持久化其完整的跨数据库可移植存储清单。
    ///
    /// # Errors
    /// 若已发布版本的 SQL 校验和被篡改、迁移历史存在断层、表前缀与其他插件冲突，或执行后的实际库表结构与声明发生漂移，则报错并回滚。
    pub async fn register_plugin_storage(
        &self,
        plugin_id: &str,
        code_version: &str,
        declaration: &PluginStorageDeclaration,
    ) -> Result<()> {
        let manifest = declaration.normalized_manifest()?;
        if plugin_id.is_empty()
            || plugin_id.len() > 64
            || code_version.is_empty()
            || !plugin_id.as_bytes()[0].is_ascii_alphabetic()
            || !plugin_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            || nuono_domain::deployment::is_reserved_plugin_name(plugin_id)
            || matches!(
                plugin_id,
                "backup" | "plugin-jobs" | "store-jobs" | "runtime_tasks"
            )
        {
            return Err(invalid("插件身份无效或占用框架持久/缓存目录"));
        }
        manifest_tables(&manifest)?;
        for asset in &manifest.assets {
            match asset {
                nuono_app::plugin_manifest::PersistentAsset::DataDirectory { path, .. } => {
                    super::files::validate_relative(path)?;
                    if path.split('/').next() != Some(plugin_id) {
                        return Err(invalid("插件持久文件必须位于自身 data 根"));
                    }
                }
                nuono_app::plugin_manifest::PersistentAsset::External { path, .. }
                | nuono_app::plugin_manifest::PersistentAsset::Reference { path, .. } => {
                    if !std::path::Path::new(path).is_absolute() {
                        return Err(invalid("外置资产必须有实际绝对路径"));
                    }
                }
            }
        }
        let tx = crate::db::migrations::migration_transaction(self.orm()).await?;
        let before = registered_schema(&tx).await?;
        super::transfer::inventory(&tx, &before, false).await?;
        super::structure::verify_structure(&tx, &before).await?;
        let now = chrono::Utc::now().timestamp_millis();
        tx.execute_raw(Statement::from_sql_and_values(self.backend(),
            "INSERT INTO nuono_plugins(plugin_id,created_at_ms,updated_at_ms) VALUES($1,$2,$2) ON CONFLICT(plugin_id) DO NOTHING",[plugin_id.into(),now.into()])).await?;
        let registered = registered_plugins(&tx).await?;
        for plugin in &registered {
            if plugin.plugin_id == plugin_id {
                if plugin.version.is_some() && plugin.manifest.table_prefix != manifest.table_prefix
                {
                    return Err(invalid("已有迁移历史的插件表前缀不能改变"));
                }
            } else if plugin.manifest.table_prefix == manifest.table_prefix
                || !manifest_tables(&plugin.manifest)?.is_disjoint(&manifest_tables(&manifest)?)
            {
                return Err(invalid("插件表前缀或表归属与其他插件冲突"));
            }
        }
        let mut empty = PortableManifest::empty(plugin_id);
        empty.table_prefix = manifest.table_prefix.clone();
        tx.execute_raw(Statement::from_sql_and_values(self.backend(),
            "UPDATE nuono_plugins SET table_prefix=COALESCE(table_prefix,$1),code_version=COALESCE(code_version,$2),installed=$3,portable_manifest_json=COALESCE(portable_manifest_json,$4),registry_updated_at_ms=$5 WHERE plugin_id=$6",
            [manifest.table_prefix.clone().into(),code_version.into(),
                if self.backend()==DbBackend::Postgres{true.into()}else{1_i64.into()},serde_json::to_string(&empty)?.into(),now.into(),plugin_id.into()])).await?;
        let rows=tx.query_all_raw(Statement::from_sql_and_values(self.backend(),
            "SELECT version,description,checksum FROM nuono_plugin_migrations WHERE plugin_id=$1 ORDER BY version",[plugin_id.into()])).await?;
        let mut applied = std::collections::BTreeMap::new();
        for row in rows {
            applied.insert(row.try_get::<i64>("", "version")?, row);
        }
        let mut versions: Vec<_> = declaration.versions.iter().collect();
        versions.sort_by_key(|v| v.version);
        if applied
            .keys()
            .any(|id| !versions.iter().any(|v| v.version == *id))
        {
            return Err(invalid("插件声明遗漏已应用版本或试图降级"));
        }
        for version in versions {
            let digest = version.checksum()?;
            if let Some(row) = applied.get(&version.version) {
                if row.try_get::<String>("", "description")? != version.description
                    || row.try_get::<String>("", "checksum")? != digest
                {
                    return Err(invalid(format!(
                        "插件 {plugin_id} 已发布版本 {} 被改写",
                        version.version
                    )));
                }
                continue;
            }
            if applied.keys().any(|v| *v > version.version) {
                return Err(invalid("插件版本历史存在空洞"));
            }
            let sql = match self.backend() {
                DbBackend::Postgres => &version.postgres_sql,
                DbBackend::Sqlite => &version.sqlite_sql,
                _ => return Err(invalid("不支持的后端")),
            };
            let previous = super::structure::snapshot(&tx).await?;
            tx.execute_unprepared(sql).await?;
            if previous == super::structure::snapshot(&tx).await? {
                return Err(invalid(format!(
                    "插件 {plugin_id} 的迁移 {} 没有产生实际结构变更",
                    version.version
                )));
            }
            tx.execute_raw(Statement::from_sql_and_values(self.backend(),
                "INSERT INTO nuono_plugin_migrations(plugin_id,version,description,checksum,applied_at_ms) VALUES($1,$2,$3,$4,$5)",
                [plugin_id.into(),version.version.into(),version.description.clone().into(),digest.into(),chrono::Utc::now().timestamp_millis().into()])).await?;
        }
        let version = declaration.versions.last().map(|v| v.version);
        tx.execute_raw(Statement::from_sql_and_values(self.backend(),
            "UPDATE nuono_plugins SET schema_version=$1,code_version=$2,installed=$3,portable_manifest_json=$4,registry_updated_at_ms=$5,table_prefix=$6 WHERE plugin_id=$7",
            [version.into(),code_version.into(),if self.backend()==DbBackend::Postgres{true.into()}else{1_i64.into()},
                serde_json::to_string(&manifest)?.into(),now.into(),manifest.table_prefix.clone().into(),plugin_id.into()])).await?;
        let schema = registered_schema(&tx).await?;
        super::transfer::inventory(&tx, &schema, false).await?;
        super::structure::verify_structure(&tx, &schema).await?;
        tx.commit().await?;
        Ok(())
    }

    /// 将指定插件标记为「当前未加载插件代码（`installed = false`）」，但完整保留其业务表数据、迁移版本历史与存储归属声明。
    ///
    /// # Errors
    /// 若数据库更新失败则返回错误；本操作绝不删除任何业务行或磁盘文件。
    pub async fn mark_plugin_code_absent(&self, plugin_id: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        self.orm()
            .execute_raw(Statement::from_sql_and_values(
                self.backend(),
                "UPDATE nuono_plugins SET installed=$1,registry_updated_at_ms=$2 WHERE plugin_id=$3 AND table_prefix IS NOT NULL",
                [
                    if self.backend() == DbBackend::Postgres {
                        false.into()
                    } else {
                        0_i64.into()
                    },
                    now.into(),
                    plugin_id.into(),
                ],
            ))
            .await?;
        Ok(())
    }

    /// 根据当前可执行文件实际搭载的完整插件列表批量对账更新 `installed` 状态（不删除任何已卸载插件保留的数据库记录或文件）。
    ///
    /// # Errors
    /// 若数据库写入失败则原子回滚整次状态更新。
    pub async fn reconcile_plugin_installations(
        &self,
        installed: &[String],
    ) -> Result<Vec<String>> {
        let tx = self.begin_write().await?;
        let rows = tx
            .query_all_raw(Statement::from_string(
                self.backend(),
                "SELECT plugin_id FROM nuono_plugins WHERE table_prefix IS NOT NULL ORDER BY plugin_id",
            ))
            .await?;
        let mut absent = Vec::new();
        for row in rows {
            let plugin_id = row.try_get::<String>("", "plugin_id")?;
            if !installed.contains(&plugin_id) {
                let now = chrono::Utc::now().timestamp_millis();
                tx.execute_raw(Statement::from_sql_and_values(self.backend(),
                    "UPDATE nuono_plugins SET installed=$1,registry_updated_at_ms=$2 WHERE plugin_id=$3",
                    [if self.backend()==DbBackend::Postgres{false.into()}else{0_i64.into()},now.into(),plugin_id.clone().into()])).await?;
                absent.push(plugin_id);
            }
        }
        tx.commit().await?;
        Ok(absent)
    }
}
