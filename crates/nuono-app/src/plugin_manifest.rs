//! 插件持久化存储清单：声明插件数据表结构、文件资产目录与账号合并策略。

use std::collections::BTreeSet;

mod checksum;
mod naming;
pub use checksum::migration_checksum;
pub use naming::{normalize_table_prefix, validate_table_prefix};

use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

use crate::plugin_data::PluginMigrationForBackend;

/// 账号合并时，插件业务表中账号引用列的处理策略。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountMergePolicy {
    FollowAccount,
    RetainHistory,
    Reject,
}

/// 插件业务表中引用框架账号 ID 的列声明。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountReference {
    pub table: String,
    pub column: String,
    pub merge_policy: AccountMergePolicy,
}

/// 插件持久化文件目录声明（外部目录在跨环境迁移时需显式指定路径映射）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "location", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistentAsset {
    DataDirectory {
        key: String,
        path: String,
    },
    External {
        key: String,
        path: String,
        required: bool,
    },
    Reference {
        key: String,
        path: String,
        required: bool,
    },
}

/// 插件当前生效的可移植存储清单（独立于历史迁移脚本与已安装代码保存）。
/// 其中的 Schema SQL 仅限声明式的 `CREATE TABLE` / `CREATE INDEX`，宿主会校验所有归属对象的表名前缀。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableManifest {
    pub format: u32,
    pub table_prefix: String,
    pub postgres_schema: String,
    pub sqlite_schema: String,
    pub assets: Vec<PersistentAsset>,
    pub account_references: Vec<AccountReference>,
    #[serde(default, with = "nuono_domain::scalar::optional_i64_string")]
    pub merge_participant_version: Option<i64>,
}

impl PortableManifest {
    /// 构造不包含自定义业务表、仅包含默认持久化数据目录的空存储清单。
    pub fn empty(plugin_id: &str) -> Self {
        Self {
            format: 1,
            table_prefix: naming::snake_case_prefix(plugin_id),
            postgres_schema: String::new(),
            sqlite_schema: String::new(),
            assets: vec![PersistentAsset::DataDirectory {
                key: "data".into(),
                path: plugin_id.into(),
            }],
            account_references: Vec::new(),
            merge_participant_version: None,
        }
    }
}

/// 单条不可变的插件数据库版本迁移记录（当前生效的完整表结构清单则在 `PortableManifest` 中单独声明）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStorageVersion {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub version: i64,
    pub description: String,
    pub postgres_sql: String,
    pub sqlite_sql: String,
}

/// 插件的完整存储声明（包含当前存储清单以及按版本号升序排列的历史迁移列表）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStorageDeclaration {
    pub manifest: PortableManifest,
    pub versions: Vec<PluginStorageVersion>,
}

impl PluginStorageDeclaration {
    /// 从仅包含建表语句（`CREATE`）的迁移列表自动生成存储声明（若包含 `ALTER` 等修改语句，则需显式提供当前最新的完整 `PortableManifest`）。
    ///
    /// # Errors
    /// 在执行任何数据库写入前拒绝无效或重复的迁移版本号。
    pub fn from_migrations(
        plugin_id: &str,
        migrations: Vec<PluginMigrationForBackend>,
    ) -> Result<Self> {
        PluginMigrationForBackend::validate_all(&migrations)?;
        let mut migrations = migrations;
        migrations.sort_by_key(|migration| migration.version);
        let mut manifest = PortableManifest::empty(plugin_id);
        let mut versions = Vec::new();
        for migration in migrations {
            manifest.postgres_schema.push_str(&migration.pg_sql);
            manifest.postgres_schema.push_str(";\n");
            manifest.sqlite_schema.push_str(&migration.sqlite_sql);
            manifest.sqlite_schema.push_str(";\n");
            versions.push(PluginStorageVersion {
                version: migration.version,
                description: migration.description,
                postgres_sql: migration.pg_sql,
                sqlite_sql: migration.sqlite_sql,
            });
        }
        Ok(Self { manifest, versions })
    }

    /// 静态校验版本号递增性与存储清单元数据完整性（不执行任何插件生命周期代码）。
    ///
    /// # Errors
    /// 当存在重复版本号、不支持的清单格式或无效的资产路径标识时返回错误。
    pub fn validate(&self) -> Result<()> {
        normalize_table_prefix(&self.manifest.table_prefix)?;
        let mut versions = BTreeSet::new();
        let mut previous = 0;
        for item in &self.versions {
            if item.version <= previous
                || !versions.insert(item.version)
                || item.description.trim().is_empty()
                || item.postgres_sql.trim().is_empty()
                || item.sqlite_sql.trim().is_empty()
            {
                return Err(NuonoError::InvalidInput(
                    "存储版本必须为唯一正整数，说明和迁移 SQL 不能为空".into(),
                ));
            }
            previous = item.version;
        }
        let manifest = &self.manifest;
        if manifest.format != 1 || manifest.merge_participant_version.is_some_and(|v| v <= 0) {
            return Err(NuonoError::InvalidInput(
                "不支持的存储清单/合并参与者版本".into(),
            ));
        }
        if self.versions.is_empty()
            && (!manifest.postgres_schema.trim().is_empty()
                || !manifest.sqlite_schema.trim().is_empty())
        {
            return Err(NuonoError::InvalidInput(
                "业务表声明必须提供实际迁移历史".into(),
            ));
        }
        let mut assets = BTreeSet::new();
        for asset in &manifest.assets {
            let (key, path) = match asset {
                PersistentAsset::DataDirectory { key, path }
                | PersistentAsset::External { key, path, .. }
                | PersistentAsset::Reference { key, path, .. } => (key, path),
            };
            if key.is_empty()
                || key.len() > 64
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                || !assets.insert(key)
                || path.is_empty()
                || path.contains('\0')
            {
                return Err(NuonoError::InvalidInput("持久资产键/路径无效或重复".into()));
            }
        }
        Ok(())
    }

    /// 返回表名前缀已规范化为 `snake_case_` 并通过校验的存储清单。
    ///
    /// # Errors
    /// 在持久化前拒绝无效的存储声明或非法的表名前缀。
    pub fn normalized_manifest(&self) -> Result<PortableManifest> {
        self.validate()?;
        let mut manifest = self.manifest.clone();
        manifest.table_prefix = normalize_table_prefix(&manifest.table_prefix)?;
        Ok(manifest)
    }
}

impl PluginStorageVersion {
    /// 计算跨数据库后端一致的规范化迁移文档校验和。
    ///
    /// # Errors
    /// 当版本元数据无效或序列化失败时返回错误。
    pub fn checksum(&self) -> Result<String> {
        migration_checksum(
            self.version,
            &self.description,
            &[
                ("postgres", &self.postgres_sql),
                ("sqlite", &self.sqlite_sql),
            ],
        )
    }
}
