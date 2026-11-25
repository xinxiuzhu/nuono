//! 插件持久化数据的离线显式清理（Purge）：支持先生成审查计划，并在执行中断时通过持久化日志恢复数据库与隔离区文件。

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use nuono_app::plugin_manifest::PersistentAsset;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{external, files, invalid, quoted};
use crate::error::Result;

pub const PENDING: &str = ".portable-purge-pending";
pub const JOURNAL: &str = ".portable-purge-journal.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgeRoot {
    pub path: PathBuf,
    pub directory: bool,
    pub files: Vec<files::Asset>,
}

/// 可供管理员预先审查的插件数据清理计划（精确列出归属表、外键依赖、待删文件与受影响授权数；若执行前底层数据发生任何变动则该计划自动失效）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PurgePlan {
    pub operation_id: String,
    pub plugin_id: String,
    pub deployment_id: String,
    pub database_sha256: String,
    pub tables: Vec<String>,
    pub dependencies: Vec<String>,
    pub files: Vec<PurgeRoot>,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub affected_authorizations: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    plan: PurgePlan,
    quarantine: Vec<PathBuf>,
    database_committed: bool,
    complete: bool,
}

/// 生成指定插件的数据清理预览计划（只读扫描，不修改任何数据库记录或磁盘文件；调用方须持有离线维护锁）。
///
/// # Errors
/// 若存在未完成的维护操作、表结构与声明不符、资产路径与其他插件/框架重叠或必需资产缺失，则返回错误。
pub async fn plan(conn: &DatabaseConnection, plugin_id: &str, data: &Path) -> Result<PurgePlan> {
    let operation = conn
        .query_one_raw(Statement::from_string(
            conn.get_database_backend(),
            "SELECT maintenance_operation FROM nuono_deployment WHERE id=1",
        ))
        .await?
        .ok_or_else(|| invalid("缺少部署身份"))?
        .try_get::<Option<String>>("", "maintenance_operation")?;
    if operation.is_some() {
        return Err(invalid("数据库已有未完成维护，不能另开清除计划"));
    }
    let schema = super::registered_schema(conn).await?;
    let image = super::export(conn, &schema).await?;
    let plugins = super::registered_plugins(conn).await?;
    let plugin = plugins
        .iter()
        .find(|p| p.plugin_id == plugin_id)
        .ok_or_else(|| invalid("插件没有持久登记"))?;
    let tables = super::manifest_tables(&plugin.manifest)?;
    let mut dependencies = Vec::new();
    for table in schema.tables.keys().filter(|t| !tables.contains(*t)) {
        if conn.get_database_backend() == DbBackend::Sqlite {
            for row in conn
                .query_all_raw(Statement::from_string(
                    DbBackend::Sqlite,
                    format!("PRAGMA foreign_key_list({})", quoted(table)?),
                ))
                .await?
            {
                let target = row.try_get::<String>("", "table")?;
                if tables.contains(&target) {
                    dependencies.push(format!(
                        "{table}.{} -> {target}",
                        row.try_get::<String>("", "from")?
                    ));
                }
            }
        } else {
            for row in conn.query_all_raw(Statement::from_sql_and_values(DbBackend::Postgres,
                "SELECT destination.relname AS target,k.conname FROM pg_constraint k JOIN pg_class origin ON origin.oid=k.conrelid JOIN pg_namespace n ON n.oid=origin.relnamespace JOIN pg_class destination ON destination.oid=k.confrelid WHERE k.contype='f' AND n.nspname='public' AND origin.relname=$1",[table.clone().into()])).await? {
                let target=row.try_get::<String>("","target")?;
                if tables.contains(&target){dependencies.push(format!("{table}.{} -> {target}",row.try_get::<String>("","conname")?));}
            }
        }
    }
    let mut roots = Vec::<PurgeRoot>::new();
    for asset in &plugin.manifest.assets {
        if let Some(path) = asset_path(data, plugin_id, asset)? {
            for root in &roots {
                if path.starts_with(&root.path) || root.path.starts_with(&path) {
                    return Err(invalid("同一插件的清除资产根不能重叠"));
                }
            }
            for declaration in &image
                .deployment
                .as_ref()
                .ok_or_else(|| invalid("缺少部署身份"))?
                .assets
            {
                if let Some(other_path) = dependency_path(data, "nuono", declaration)?
                    && (path.starts_with(&other_path) || other_path.starts_with(&path))
                {
                    return Err(invalid("清除资产与框架登记文件重叠"));
                }
            }
            for other in plugins.iter().filter(|p| p.plugin_id != plugin_id) {
                for declaration in &other.manifest.assets {
                    if let Some(other_path) = dependency_path(data, &other.plugin_id, declaration)?
                        && (path.starts_with(&other_path) || other_path.starts_with(&path))
                    {
                        return Err(invalid("清除资产与其他插件登记根重叠"));
                    }
                }
            }
            if path.exists() {
                roots.push(inventory_root(&path)?);
            }
        }
    }
    let affected=conn.query_one_raw(Statement::from_sql_and_values(conn.get_database_backend(),
        "SELECT COUNT(*) AS n FROM nuono_group_authorizations WHERE (plugin_id=$1 OR issuer_plugin_id=$1) AND revoked_at_ms IS NULL",[plugin_id.into()])).await?.ok_or_else(||invalid("无法读取授权影响"))?.try_get::<i64>("","n")?;
    Ok(PurgePlan {
        operation_id: uuid::Uuid::new_v4().to_string(),
        plugin_id: plugin_id.into(),
        deployment_id: image
            .deployment
            .as_ref()
            .ok_or_else(|| invalid("缺少部署身份"))?
            .deployment_id
            .clone(),
        database_sha256: hex::encode(Sha256::digest(serde_json::to_vec(&image)?)),
        tables: tables.into_iter().collect(),
        dependencies,
        files: roots,
        affected_authorizations: affected,
    })
}

/// 严格按照已确认的 `PurgePlan` 执行插件数据清理：先将关联文件移入隔离区，再在单个数据库事务内删除插件表、清理注册台账并写入审计日志，最后彻底删除隔离区文件。
///
/// # Errors
/// 若存在未解除的外部外键依赖，或生成计划后数据库/文件发生了任何变动，则拒绝执行；若执行中途中断，会保留恢复日志供 `recover` 续跑或回滚。
pub async fn apply(conn: &DatabaseConnection, data: &Path, reviewed: &PurgePlan) -> Result<()> {
    uuid::Uuid::parse_str(&reviewed.operation_id).map_err(|_| invalid("清除操作标识无效"))?;
    if !reviewed.dependencies.is_empty() {
        return Err(invalid("存在外部外键依赖，必须先显式处理依赖"));
    }
    if data.join(PENDING).exists() {
        return Err(invalid("已有清除回执，请先恢复"));
    }
    if data.join(JOURNAL).exists() {
        let previous: Journal =
            super::parse_document(&files::read_bounded(&data.join(JOURNAL), 4 * 1024 * 1024)?)?;
        if !previous.complete {
            return Err(invalid("已有未完成清除回执，请先恢复"));
        }
        uuid::Uuid::parse_str(&previous.plan.operation_id)
            .map_err(|_| invalid("旧清除回执标识无效"))?;
        let archive = data.join(".portable-purge-history");
        files::private_dir(&archive)?;
        let archived = archive.join(format!("{}.json", previous.plan.operation_id));
        if archived.exists() {
            return Err(invalid("清除回执归档发生冲突"));
        }
        fs::rename(data.join(JOURNAL), &archived).map_err(files::io)?;
        files::sync_directory(&archive)?;
        files::sync_directory(data)?;
    }
    let current = plan(conn, &reviewed.plugin_id, data).await?;
    if current.database_sha256 != reviewed.database_sha256
        || current.deployment_id != reviewed.deployment_id
        || current.affected_authorizations != reviewed.affected_authorizations
        || current.tables != reviewed.tables
        || serde_json::to_value(&current.files)? != serde_json::to_value(&reviewed.files)?
        || !current.dependencies.is_empty()
    {
        return Err(invalid("清除计划已过期，需重新预览"));
    }
    let mut journal = Journal {
        plan: reviewed.clone(),
        quarantine: reviewed
            .files
            .iter()
            .enumerate()
            .map(|(index, root)| {
                root.path
                    .with_file_name(format!(".nuono-purge-{}-{index}", reviewed.operation_id))
            })
            .collect(),
        database_committed: false,
        complete: false,
    };
    if journal.quarantine.iter().any(|p| p.exists()) {
        return Err(invalid("清除暂存路径已存在"));
    }
    external::atomic_json(&data.join(JOURNAL), &journal)?;
    files::write_new(&data.join(PENDING), reviewed.operation_id.as_bytes())?;
    files::sync_directory(data)?;
    super::set_maintenance(conn, None, Some(&reviewed.operation_id)).await?;
    for (root, quarantine) in reviewed.files.iter().zip(&journal.quarantine) {
        if serde_json::to_value(inventory_root(&root.path)?)? != serde_json::to_value(root)? {
            return Err(invalid("清除资产发生变化"));
        }
        fs::rename(&root.path, quarantine).map_err(files::io)?;
        sync_parent(&root.path)?;
    }
    let tx = crate::db::migrations::migration_transaction(conn).await?;
    if conn.get_database_backend() == DbBackend::Sqlite {
        tx.execute_unprepared("PRAGMA defer_foreign_keys=ON")
            .await?;
    }
    if !reviewed.tables.is_empty() {
        if conn.get_database_backend() == DbBackend::Postgres {
            tx.execute_unprepared(&format!(
                "DROP TABLE {}",
                reviewed
                    .tables
                    .iter()
                    .map(|t| quoted(t))
                    .collect::<Result<Vec<_>>>()?
                    .join(",")
            ))
            .await?;
        } else {
            for table in &reviewed.tables {
                tx.execute_unprepared(&format!("DROP TABLE {}", quoted(table)?))
                    .await?;
            }
        }
    }
    for table in [
        "nuono_plugin_overrides",
        "nuono_plugin_kv",
        "nuono_plugin_migrations",
    ] {
        tx.execute_raw(Statement::from_sql_and_values(
            conn.get_database_backend(),
            format!("DELETE FROM {table} WHERE plugin_id=$1"),
            [reviewed.plugin_id.clone().into()],
        ))
        .await?;
    }
    super::catalog::clear_storage_registration(&tx, &reviewed.plugin_id).await?;
    tx.execute_raw(Statement::from_sql_and_values(
        conn.get_database_backend(),
        "DELETE FROM nuono_runtime_secrets WHERE owner_plugin_id=$1",
        [reviewed.plugin_id.clone().into()],
    ))
    .await?;
    let now = chrono::Utc::now().timestamp_millis();
    tx.execute_raw(Statement::from_sql_and_values(conn.get_database_backend(),
        "UPDATE nuono_group_authorizations SET revoked_at_ms=$1,updated_at_ms=$1,revision=revision+1 WHERE (plugin_id=$2 OR issuer_plugin_id=$2) AND revoked_at_ms IS NULL",[now.into(),reviewed.plugin_id.clone().into()])).await?;
    tx.execute_raw(Statement::from_sql_and_values(conn.get_database_backend(),
        "UPDATE nuono_plugins SET enabled=$1,allow_user_overrides=$1,allow_group_overrides=$1,config_json='{}',service_config_json='{}',trigger_policy_json=NULL,slow_threshold_ms=NULL,revision=revision+1,updated_at_ms=$2 WHERE plugin_id=$3",
        [if conn.get_database_backend()==DbBackend::Postgres{false.into()}else{0_i64.into()},now.into(),reviewed.plugin_id.clone().into()])).await?;
    tx.execute_raw(Statement::from_sql_and_values(conn.get_database_backend(),
        "INSERT INTO nuono_audit_events(occurred_at_ms,action,result,operation_id,details_json) VALUES($1,'plugin_data_purged','success',$2,$3)",
        [now.into(),reviewed.operation_id.clone().into(),serde_json::json!({"plugin_id":reviewed.plugin_id,"tables":reviewed.tables,"revoked_authorizations":reviewed.affected_authorizations}).to_string().into()])).await?;
    let schema = super::registered_schema(&tx).await?;
    super::structure::verify_structure(&tx, &schema).await?;
    if conn.get_database_backend() == DbBackend::Sqlite
        && !tx
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "PRAGMA foreign_key_check",
            ))
            .await?
            .is_empty()
    {
        return Err(invalid("清除后外键不完整"));
    }
    tx.commit().await?;
    journal.database_committed = true;
    external::atomic_json(&data.join(JOURNAL), &journal)?;
    finish(conn, data, &mut journal).await
}

/// 恢复意外中断的插件数据清理操作：若数据库事务已提交则继续删净隔离区文件并解除维护状态；若数据库事务已回滚则将隔离区文件原样还原回原路径。
///
/// # Errors
/// 若当前连接的数据库部署身份与恢复日志不符，或隔离区文件被外部篡改，则报错拒绝，绝不覆盖现有文件。
pub async fn recover(conn: &DatabaseConnection, data: &Path) -> Result<()> {
    let mut journal: Journal =
        super::parse_document(&files::read_bounded(&data.join(JOURNAL), 4 * 1024 * 1024)?)?;
    uuid::Uuid::parse_str(&journal.plan.operation_id)
        .map_err(|_| invalid("清除回执操作标识无效"))?;
    let expected = journal
        .plan
        .files
        .iter()
        .enumerate()
        .map(|(index, root)| {
            root.path.with_file_name(format!(
                ".nuono-purge-{}-{index}",
                journal.plan.operation_id
            ))
        })
        .collect::<Vec<_>>();
    if expected != journal.quarantine {
        return Err(invalid("清除隔离路径与登记操作不一致"));
    }
    let row = conn
        .query_one_raw(Statement::from_string(
            conn.get_database_backend(),
            "SELECT deployment_id,maintenance_operation FROM nuono_deployment WHERE id=1",
        ))
        .await?
        .ok_or_else(|| invalid("缺少部署身份"))?;
    if row.try_get::<String>("", "deployment_id")? != journal.plan.deployment_id {
        return Err(invalid("清除回执属于其他部署"));
    }
    let fence = row.try_get::<Option<String>>("", "maintenance_operation")?;
    if fence.is_some() && fence.as_deref() != Some(&journal.plan.operation_id) {
        return Err(invalid("清除恢复维护标识不匹配"));
    }
    if journal.complete && fence.is_none() {
        return clear_marker(conn, data, &journal).await;
    }
    let committed=conn.query_one_raw(Statement::from_sql_and_values(conn.get_database_backend(),
        "SELECT id FROM nuono_audit_events WHERE action='plugin_data_purged' AND operation_id=$1",[journal.plan.operation_id.clone().into()])).await?.is_some();
    if committed {
        if fence.is_none() && !journal.complete {
            return Err(invalid("已提交清除丢失维护标识"));
        }
        journal.database_committed = true;
        return finish(conn, data, &mut journal).await;
    }
    for (root, quarantine) in journal.plan.files.iter().zip(&journal.quarantine) {
        if quarantine.exists() {
            if root.path.exists() {
                return Err(invalid("原资产位置已被占用，不能回退"));
            }
            let actual = inventory_root(quarantine)?;
            if actual.directory != root.directory
                || serde_json::to_value(&actual.files)? != serde_json::to_value(&root.files)?
            {
                return Err(invalid("隔离资产已改变，不能回退"));
            }
            fs::rename(quarantine, &root.path).map_err(files::io)?;
            sync_parent(&root.path)?;
        }
    }
    journal.complete = true;
    external::atomic_json(&data.join(JOURNAL), &journal)?;
    clear_marker(conn, data, &journal).await
}

async fn finish(conn: &DatabaseConnection, data: &Path, journal: &mut Journal) -> Result<()> {
    for (root, quarantine) in journal.plan.files.iter().zip(&journal.quarantine) {
        if !quarantine.exists() {
            continue;
        }
        let remaining = inventory_root(quarantine)?;
        for file in &remaining.files {
            if !root.files.iter().any(|known| {
                known.path == file.path && known.sha256 == file.sha256 && known.size == file.size
            }) {
                return Err(invalid("清除暂存目录出现未登记内容"));
            }
        }
        if root.directory {
            fs::remove_dir_all(quarantine).map_err(files::io)?;
        } else {
            files::regular(quarantine)?;
            fs::remove_file(quarantine).map_err(files::io)?;
        }
        sync_parent(quarantine)?;
    }
    journal.complete = true;
    external::atomic_json(&data.join(JOURNAL), journal)?;
    clear_marker(conn, data, journal).await
}

async fn clear_marker(conn: &DatabaseConnection, data: &Path, journal: &Journal) -> Result<()> {
    if data.join(PENDING).exists() {
        if files::read_bounded(&data.join(PENDING), 128)? != journal.plan.operation_id.as_bytes() {
            return Err(invalid("清除保护标记不匹配"));
        }
        fs::remove_file(data.join(PENDING)).map_err(files::io)?;
        files::sync_directory(data)?;
    }
    let current = conn
        .query_one_raw(Statement::from_string(
            conn.get_database_backend(),
            "SELECT maintenance_operation FROM nuono_deployment WHERE id=1",
        ))
        .await?
        .ok_or_else(|| invalid("缺少部署身份"))?
        .try_get::<Option<String>>("", "maintenance_operation")?;
    if current.is_some() {
        super::set_maintenance(conn, Some(&journal.plan.operation_id), None).await?;
    }
    Ok(())
}

fn asset_path(data: &Path, owner: &str, asset: &PersistentAsset) -> Result<Option<PathBuf>> {
    match asset {
        PersistentAsset::DataDirectory { path, .. } => {
            files::validate_relative(path)?;
            if path.split('/').next() != Some(owner) {
                return Err(invalid("清除范围超出插件专属 data 根"));
            }
            Ok(Some(data.canonicalize().map_err(files::io)?.join(path)))
        }
        PersistentAsset::External {
            key,
            path,
            required,
        } => {
            let actual = canonical_path(&external::resolved_path(
                data,
                &format!("{owner}/{key}"),
                Path::new(path),
            )?)?;
            if *required && !actual.exists() {
                return Err(invalid("登记的必需外置文件缺失"));
            }
            let data = data.canonicalize().map_err(files::io)?;
            if actual.starts_with(&data) || data.starts_with(&actual) {
                return Err(invalid(
                    "外置资产与框架 data 根重叠；应登记插件专属 DataDirectory",
                ));
            }
            Ok(Some(actual))
        }
        PersistentAsset::Reference { .. } => Ok(None),
    }
}

fn dependency_path(data: &Path, owner: &str, asset: &PersistentAsset) -> Result<Option<PathBuf>> {
    if let PersistentAsset::Reference { path, .. } = asset {
        return Ok(Some(canonical_path(Path::new(path))?));
    }
    asset_path(data, owner, asset)
}

fn canonical_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(invalid("资产必须是无 .. 的绝对路径"));
    }
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(invalid("资产根不能是符号链接"));
    }
    let ancestor = path
        .ancestors()
        .find(|p| p.exists())
        .ok_or_else(|| invalid("资产路径不存在可解析父目录"))?;
    let canonical = ancestor.canonicalize().map_err(files::io)?;
    let suffix = path
        .strip_prefix(ancestor)
        .map_err(|e| invalid(e.to_string()))?;
    Ok(if suffix.as_os_str().is_empty() {
        canonical
    } else {
        canonical.join(suffix)
    })
}

fn inventory_root(path: &Path) -> Result<PurgeRoot> {
    let metadata = fs::symlink_metadata(path).map_err(files::io)?;
    if metadata.file_type().is_symlink() {
        return Err(invalid("清除资产根不能为符号链接"));
    }
    let directory = metadata.is_dir();
    let names = if directory {
        files::collect(path, &BTreeSet::new())?
    } else {
        files::regular(path)?;
        vec!["value".into()]
    };
    let mut files_list = Vec::new();
    for name in names {
        let file = if directory {
            path.join(&name)
        } else {
            path.into()
        };
        files_list.push(files::Asset {
            path: name,
            size: files::regular(&file)?.len(),
            sha256: files::hash_file(&file)?,
        });
    }
    Ok(PurgeRoot {
        path: path.into(),
        directory,
        files: files_list,
    })
}
fn sync_parent(path: &Path) -> Result<()> {
    files::sync_directory(path.parent().ok_or_else(|| invalid("清除路径缺少父目录"))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_app::{
        plugin_data::PluginMigrationForBackend, plugin_manifest::PluginStorageDeclaration,
    };
    async fn fixture() -> (crate::Database, PathBuf, PluginStorageDeclaration, Journal) {
        let data =
            std::env::temp_dir().join(format!("nuono-purge-recovery-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&data).unwrap();
        let data = data.canonicalize().unwrap();
        fs::create_dir(data.join("recovery")).unwrap();
        fs::write(data.join("recovery/value"), b"kept").unwrap();
        let db = crate::test_support::test_db(1).await;
        let declaration = PluginStorageDeclaration::from_migrations(
            "recovery",
            vec![PluginMigrationForBackend {
                version: 1,
                description: "initial".into(),
                pg_sql: "CREATE TABLE recovery_rows(id BIGINT PRIMARY KEY)".into(),
                sqlite_sql: "CREATE TABLE recovery_rows(id INTEGER PRIMARY KEY) STRICT".into(),
            }],
        )
        .unwrap();
        db.register_plugin_storage("recovery", "1", &declaration)
            .await
            .unwrap();
        db.orm()
            .execute_unprepared("INSERT INTO recovery_rows VALUES(1)")
            .await
            .unwrap();
        let plan = plan(db.orm(), "recovery", &data).await.unwrap();
        let quarantine = plan
            .files
            .iter()
            .enumerate()
            .map(|(i, r)| {
                r.path
                    .with_file_name(format!(".nuono-purge-{}-{i}", plan.operation_id))
            })
            .collect();
        let journal = Journal {
            plan,
            quarantine,
            database_committed: false,
            complete: false,
        };
        external::atomic_json(&data.join(JOURNAL), &journal).unwrap();
        files::write_new(&data.join(PENDING), journal.plan.operation_id.as_bytes()).unwrap();
        (db, data, declaration, journal)
    }
    #[tokio::test]
    async fn interruption_before_fence_or_before_commit_restores_only_owned_files() {
        for moved in [false, true] {
            let (db, data, _declaration, journal) = fixture().await;
            if moved {
                super::super::set_maintenance(db.orm(), None, Some(&journal.plan.operation_id))
                    .await
                    .unwrap();
                fs::rename(&journal.plan.files[0].path, &journal.quarantine[0]).unwrap();
            }
            recover(db.orm(), &data).await.unwrap();
            assert_eq!(fs::read(data.join("recovery/value")).unwrap(), b"kept");
            assert!(!data.join(PENDING).exists());
            assert_eq!(
                super::super::registered_plugins(db.orm()).await.unwrap()[0].version,
                Some(1)
            );
            crate::db::migrations::run_framework(db.orm())
                .await
                .unwrap();
            db.shutdown().await.unwrap();
            fs::remove_dir_all(data).unwrap();
        }
    }
    #[tokio::test]
    async fn committed_audit_receipt_resumes_deletion_even_before_file_journal_commit() {
        let (db, data, declaration, journal) = fixture().await;
        super::super::set_maintenance(db.orm(), None, Some(&journal.plan.operation_id))
            .await
            .unwrap();
        assert!(
            crate::db::migrations::run_framework(db.orm())
                .await
                .is_err()
        );
        fs::rename(&journal.plan.files[0].path, &journal.quarantine[0]).unwrap();
        let tx = crate::db::migrations::migration_transaction(db.orm())
            .await
            .unwrap();
        tx.execute_unprepared("DROP TABLE recovery_rows; DELETE FROM nuono_plugin_migrations WHERE plugin_id='recovery'").await.unwrap();
        super::super::catalog::clear_storage_registration(&tx, "recovery")
            .await
            .unwrap();
        tx.execute_raw(Statement::from_sql_and_values(db.backend(),"INSERT INTO nuono_audit_events(occurred_at_ms,action,result,operation_id,details_json) VALUES(1,'plugin_data_purged','success',$1,'{}')",[journal.plan.operation_id.clone().into()])).await.unwrap();
        tx.commit().await.unwrap();
        recover(db.orm(), &data).await.unwrap();
        assert!(!journal.quarantine[0].exists());
        assert!(!data.join("recovery").exists());
        assert!(!data.join(PENDING).exists());
        db.register_plugin_storage("recovery", "1", &declaration)
            .await
            .unwrap();
        db.shutdown().await.unwrap();
        fs::remove_dir_all(data).unwrap();
    }
}
