//! 跨数据库迁移后的目标环境插件版本门禁：在宿主启动并注册插件前，再次核对目标环境实际加载的插件代码与迁移包声明完全一致。
use super::{DatabaseImage, files, invalid};
use crate::error::Result;
use nuono_app::plugin_manifest::PluginStorageDeclaration;
use sea_orm::{ConnectionTrait, Statement};
use std::{collections::BTreeMap, path::Path};

pub const TARGET_PLUGINS: &str = ".portable-target-plugins.json";
pub type PluginVersions = BTreeMap<String, String>;

/// 从源部署导出的数据库快照中提取所有当时处于已安装状态（`installed = true`）的插件及其代码版本号清单。
pub fn installed_plugin_versions(image: &DatabaseImage) -> Result<PluginVersions> {
    let deployment = image
        .deployment
        .as_ref()
        .ok_or_else(|| invalid("缺少部署身份"))?;
    let mut versions = BTreeMap::new();
    for plugin in deployment.plugins.iter().filter(|p| p.installed) {
        let version = plugin
            .code_version
            .as_ref()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| invalid("已安装插件缺少代码版本"))?;
        if versions
            .insert(plugin.plugin_id.clone(), version.clone())
            .is_some()
        {
            return Err(invalid("插件代码清单重复"));
        }
    }
    Ok(versions)
}

/// 在目标数据目录中写入迁移包要求的插件代码版本声明文件（`.portable-target-plugins.json`）；调用方需显式提供目标环境声明的插件版本映射（此处做声明一致性校验，而非二进制签名认证）。
///
/// # Errors
/// 若目标环境声明的已安装插件列表与迁移包存在任何缺失、多余或版本号不一致，则在写入任何目标数据前直接拒绝迁移。
pub fn stage_target_plugin_versions(
    data: &Path,
    image: &DatabaseImage,
    declared: &PluginVersions,
) -> Result<()> {
    if installed_plugin_versions(image)? != *declared {
        return Err(invalid("目标部署声明的插件代码版本与迁移包不同"));
    }
    let path = data.join(TARGET_PLUGINS);
    if path.exists() {
        let previous: PluginVersions =
            super::parse_document(&files::read_bounded(&path, 4 * 1024 * 1024)?)?;
        if previous != *declared {
            return Err(invalid("暂存目标插件版本声明已改变"));
        }
    } else {
        super::external::atomic_json(&path, declared)?;
    }
    Ok(())
}

/// 在宿主启动后、执行任何插件存储写入或生命周期回调之前，核对当前可执行文件实际搭载的完整插件清单与 `.portable-target-plugins.json` 是否完全一致。
///
/// # Errors
/// 若不匹配则返回错误并保留 `.portable-target-plugins.json` 门禁文件，防止通过再次重启绕过版本检查。
pub fn verify_target_plugin_code(data: &Path, actual: &[(String, String)]) -> Result<()> {
    let path = data.join(TARGET_PLUGINS);
    if !path.exists() {
        return Ok(());
    }
    let expected: PluginVersions =
        super::parse_document(&files::read_bounded(&path, 4 * 1024 * 1024)?)?;
    let mut versions = BTreeMap::new();
    for (id, version) in actual {
        if versions.insert(id.clone(), version.clone()).is_some() {
            return Err(invalid("目标实际插件代码身份重复"));
        }
    }
    if versions != expected {
        return Err(invalid(
            "目标实际插件代码与迁移部署声明不匹配；保持停止，请部署匹配代码后重试",
        ));
    }
    Ok(())
}

/// 在任何插件注册操作修改目标数据库之前，核对每个恢复插件的代码存储声明与数据库中恢复的迁移历史台账完全一致。
///
/// # Errors
/// 只要恢复启动门禁文件存在，任何缺失、篡改或新增的插件迁移版本都会被拒绝（禁止在跨数据库迁移的同时顺带升级插件表结构）。
pub async fn verify_target_plugin_storage<C: ConnectionTrait>(
    data: &Path,
    connection: &C,
    actual: &[(&str, &PluginStorageDeclaration)],
) -> Result<()> {
    let path = data.join(TARGET_PLUGINS);
    if !path.exists() {
        return Ok(());
    }
    let expected: PluginVersions =
        super::parse_document(&files::read_bounded(&path, 4 * 1024 * 1024)?)?;
    let names = actual
        .iter()
        .map(|(id, _)| *id)
        .collect::<std::collections::BTreeSet<_>>();
    if names.len() != actual.len() || names != expected.keys().map(String::as_str).collect() {
        return Err(invalid("目标存储声明必须覆盖完整的恢复插件清单"));
    }
    let registered = super::registered_plugins(connection).await?;
    for (plugin_id, declaration) in actual {
        let manifest = declaration.normalized_manifest()?;
        let retained = registered
            .iter()
            .find(|p| p.plugin_id == *plugin_id)
            .ok_or_else(|| invalid("恢复插件缺少注册信息"))?;
        let rows = connection.query_all_raw(Statement::from_sql_and_values(
            connection.get_database_backend(),
            "SELECT version,description,checksum FROM nuono_plugin_migrations WHERE plugin_id=$1 ORDER BY version",
            [(*plugin_id).into()],
        )).await?;
        // 若插件此前曾执行过显式数据清理（purge），数据库中会保留其插件安装身份与表前缀，但已应用的迁移历史为空；
        // 这类已清空数据的插件允许在下次启动时按当前代码声明重新初始化表结构。
        if retained.manifest.table_prefix != manifest.table_prefix {
            return Err(invalid("目标插件的表前缀与恢复注册信息不同"));
        }
        if rows.is_empty() && !declaration.versions.is_empty() {
            continue;
        }
        if retained.manifest != manifest {
            return Err(invalid("目标插件的当前存储声明与恢复注册信息不同"));
        }
        if rows.len() != declaration.versions.len() {
            return Err(invalid(format!(
                "插件 {plugin_id} 的目标存储版本与恢复账本不同；迁移不能同时升级"
            )));
        }
        let mut versions = declaration.versions.iter().collect::<Vec<_>>();
        versions.sort_by_key(|version| version.version);
        for (row, declared) in rows.iter().zip(versions) {
            if row.try_get::<i64>("", "version")? != declared.version
                || row.try_get::<String>("", "description")? != declared.description
                || row.try_get::<String>("", "checksum")? != declared.checksum()?
            {
                return Err(invalid(format!(
                    "插件 {plugin_id} 的目标存储历史与恢复账本不同"
                )));
            }
        }
    }
    Ok(())
}

/// 在所有插件的代码版本与不可变存储迁移历史均核对无误后，删除 `.portable-target-plugins.json` 启动门禁文件。
///
/// # Errors
/// 若文件校验或删除失败则返回错误；在此方法成功返回前不得启动任何插件生命周期。
pub fn finish_target_plugin_code(data: &Path) -> Result<()> {
    let path = data.join(TARGET_PLUGINS);
    if path.exists() {
        files::regular(&path)?;
        std::fs::remove_file(path).map_err(files::io)?;
        files::sync_directory(data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_target_code_mismatch_keeps_startup_gate_for_retry() {
        let root = std::env::temp_dir().join(format!("nuono-target-code-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        super::super::external::atomic_json(
            &root.join(TARGET_PLUGINS),
            &BTreeMap::from([("billing", "1.0")]),
        )
        .unwrap();
        assert!(verify_target_plugin_code(&root, &[("billing".into(), "2.0".into())]).is_err());
        assert!(root.join(TARGET_PLUGINS).exists());
        assert!(verify_target_plugin_code(&root, &[]).is_err());
        verify_target_plugin_code(&root, &[("billing".into(), "1.0".into())]).unwrap();
        assert!(root.join(TARGET_PLUGINS).exists());
        finish_target_plugin_code(&root).unwrap();
        assert!(!root.join(TARGET_PLUGINS).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn restored_storage_requires_exact_history_before_any_plugin_registration() {
        let db = crate::test_support::test_db(1).await;
        let root =
            std::env::temp_dir().join(format!("nuono-target-storage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let original = PluginStorageDeclaration::from_migrations(
            "restore_gate",
            vec![nuono_app::plugin_data::PluginMigrationForBackend {
                version: 1,
                description: "gate".into(),
                pg_sql: "CREATE TABLE restore_gate_rows(id BIGINT PRIMARY KEY)".into(),
                sqlite_sql: "CREATE TABLE restore_gate_rows(id INTEGER PRIMARY KEY) STRICT".into(),
            }],
        )
        .unwrap();
        db.register_plugin_storage("restore_gate", "1.0", &original)
            .await
            .unwrap();
        super::super::external::atomic_json(
            &root.join(TARGET_PLUGINS),
            &BTreeMap::from([("restore_gate", "1.0")]),
        )
        .unwrap();
        verify_target_plugin_storage(&root, db.orm(), &[("restore_gate", &original)])
            .await
            .unwrap();
        let mut changed = original.clone();
        changed.versions[0].sqlite_sql.push_str(" -- changed");
        assert!(
            verify_target_plugin_storage(&root, db.orm(), &[("restore_gate", &changed)])
                .await
                .is_err()
        );
        let mut upgraded = original.clone();
        let mut additional = original.versions[0].clone();
        additional.version = 2;
        upgraded.versions.push(additional);
        assert!(
            verify_target_plugin_storage(&root, db.orm(), &[("restore_gate", &upgraded)])
                .await
                .is_err()
        );
        assert!(root.join(TARGET_PLUGINS).exists());
        let installed = super::super::registered_plugins(db.orm()).await.unwrap();
        assert_eq!(installed[0].version, Some(1));
        assert_eq!(installed[0].code_version.as_deref(), Some("1.0"));
        finish_target_plugin_code(&root).unwrap();
        verify_target_plugin_storage(&root, db.orm(), &[("restore_gate", &upgraded)])
            .await
            .unwrap();
        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
