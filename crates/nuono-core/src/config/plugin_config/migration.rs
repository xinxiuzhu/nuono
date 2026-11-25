//! 插件配置版本迁移处理。

use std::collections::HashMap;

use super::{PluginConfigManager, PluginConfigMigrationSource, database_values, validate_name};
use crate::config::plugin_files;
use crate::config::schema::{ConfigSchema, ConfigValue, StructuredConfig, StructuredConfigField};
use crate::error::{NuonoError, Result};

impl PluginConfigManager {
    pub async fn resolve_migration(
        &self,
        name: &str,
        source: PluginConfigMigrationSource,
    ) -> Result<()> {
        validate_name(name)?;
        let _write = self.writes.lock().await;
        let pending = self.migration_candidates.lock().await.get(name).cloned();
        let schema = match pending {
            Some(schema) => schema,
            None => self
                .schemas
                .read()
                .await
                .get(name)
                .cloned()
                .ok_or_else(|| NuonoError::ConfigLoad(format!("配置 '{name}' 未注册")))?,
        };
        if let Some(db) = &self.database {
            if matches!(source, PluginConfigMigrationSource::File) {
                let raw = tokio::fs::read_to_string(self.config_path(name))
                    .await
                    .map_err(|error| NuonoError::ConfigLoad(error.to_string()))?;
                let (_, _, values) = plugin_files::parse(&raw, &schema)?;
                let patch = serde_json::from_value(values).map_err(|error| {
                    NuonoError::ConfigLoad(format!("导入配置类型错误: {error}"))
                })?;
                self.save_database_patch_with_schema(name, patch, None, &schema)
                    .await?;
            } else {
                let saved = db.plugin_configuration(name).await?;
                database_values(&schema, &saved)?;
                // 即使显式选择了空的数据库配置，也会写入一条已提交的版本记录。
                db.save_plugin_configuration(name, &saved).await?;
                self.load_database(name, &schema).await?;
            }
            self.schemas.write().await.insert(name.to_owned(), schema);
            self.migration_candidates.lock().await.remove(name);
            return Ok(());
        }
        self.migrate_legacy_with_schema(name, Some(source), &schema)
            .await?;
        self.files
            .load(name, &self.config_path(name), &schema, true)
            .await?;
        self.migration_candidates.lock().await.remove(name);
        Ok(())
    }

    pub(super) async fn migrate_legacy(
        &self,
        name: &str,
        choice: Option<PluginConfigMigrationSource>,
    ) -> Result<()> {
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        self.migrate_legacy_with_schema(name, choice, &schema).await
    }

    pub(super) async fn migrate_legacy_with_schema(
        &self,
        _name: &str,
        _choice: Option<PluginConfigMigrationSource>,
        _schema: &ConfigSchema,
    ) -> Result<()> {
        // 运行期绝不会把数据库中的配置反向写回开发用的 TOML 文件。
        Ok(())
    }

    pub(super) async fn config_exists(&self, name: &str) -> Result<bool> {
        validate_name(name)?;
        if self.database.is_some() {
            return Ok(true);
        }
        tokio::fs::try_exists(self.config_path(name))
            .await
            .map_err(|e| NuonoError::ConfigLoad(e.to_string()))
    }

    pub(super) async fn read_structured(&self, name: &str) -> Result<StructuredConfig> {
        validate_name(name)?;
        let _write = self.writes.lock().await;
        self.read_structured_locked(name).await
    }

    pub(super) async fn read_structured_locked(&self, name: &str) -> Result<StructuredConfig> {
        if self.database.is_some() {
            if self.migration_candidates.lock().await.contains_key(name) {
                return Err(NuonoError::ConfigValidation(
                    "插件配置迁移冲突：请先明确选择导入 TOML 或使用数据库配置".into(),
                ));
            }
            let schema = self
                .schemas
                .read()
                .await
                .get(name)
                .cloned()
                .ok_or_else(|| NuonoError::ConfigLoad(format!("配置 '{name}' 未注册")))?;
            return self.load_database(name, &schema).await;
        }
        if let Err(error) = self.migrate_legacy(name, None).await {
            self.files.record_error(name, error.to_string()).await;
            return Err(error);
        }
        if !self.config_exists(name).await? && self.files.handle(name).await.is_none() {
            return Ok(HashMap::new());
        }
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        self.files
            .load(name, &self.config_path(name), &schema, false)
            .await
    }

    pub(super) async fn write_structured(
        &self,
        name: &str,
        config: &StructuredConfig,
    ) -> Result<()> {
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        self.files
            .write(name, &self.config_path(name), config, &schema)
            .await
    }

    /// 确保配置文件存在：新 key 自动补充，已有 key 保留用户值。
    ///
    /// schema 已合并服务级字段；已保存的前缀也遵循用户值优先，不按内容猜测迁移。
    pub(super) async fn ensure_config_file(&self, name: &str, schema: &ConfigSchema) -> Result<()> {
        if self.database.is_some() {
            if self.migration_candidates.lock().await.contains_key(name) {
                return Err(NuonoError::ConfigValidation(
                    "插件配置迁移冲突：请先明确选择导入 TOML 或使用数据库配置".into(),
                ));
            }
            self.load_database(name, schema).await?;
            return Ok(());
        }
        let exists = self.config_exists(name).await?;

        let mut structured = if exists {
            self.files
                .load(name, &self.config_path(name), schema, true)
                .await?
        } else {
            HashMap::new()
        };
        let mut changed = false;

        // 只补充缺失字段，已有服务设置和业务配置都保留用户值。
        for (key, field) in schema {
            if !structured.contains_key(key) {
                structured.insert(key.clone(), StructuredConfigField::from_config_field(field));
                changed = true;
            }
        }

        if !exists || changed {
            self.files
                .write(name, &self.config_path(name), &structured, schema)
                .await?;
        }

        Ok(())
    }

    pub(super) async fn ensure_database_source(&self, name: &str) -> Result<()> {
        let db = self
            .database
            .as_ref()
            .ok_or_else(|| NuonoError::ConfigLoad("未连接插件配置数据库".into()))?;
        let saved = db.plugin_configuration(name).await?;
        if saved.revision == 1
            && saved.values.is_empty()
            && saved.service.is_empty()
            && tokio::fs::try_exists(self.config_path(name))
                .await
                .map_err(|error| NuonoError::ConfigLoad(error.to_string()))?
        {
            return Err(NuonoError::ConfigValidation("插件配置迁移冲突：检测到尚未导入的 config.toml，请先明确选择导入文件或使用数据库配置".into()));
        }
        Ok(())
    }

    pub(super) async fn load_database(
        &self,
        name: &str,
        schema: &ConfigSchema,
    ) -> Result<StructuredConfig> {
        let db = self
            .database
            .as_ref()
            .ok_or_else(|| NuonoError::ConfigLoad("未连接插件配置数据库".into()))?;
        let saved = db.plugin_configuration(name).await?;
        self.validate_resources(name, &saved.values).await?;
        let version = self.versions.read().await.get(name).copied().unwrap_or(1);
        if saved.config_version != version {
            return Err(NuonoError::ConfigValidation(
                "配置格式版本与当前插件声明不一致".into(),
            ));
        }
        let structured = database_values(schema, &saved)?;
        let values = structured
            .iter()
            .map(|(key, entry)| {
                serde_json::to_value(entry.data_value()).map(|value| (key.clone(), value))
            })
            .collect::<std::result::Result<serde_json::Map<_, _>, _>>()?;
        self.files
            .publish_database(
                name,
                structured.clone(),
                values.into(),
                saved.revision.to_string(),
            )
            .await;
        Ok(structured)
    }

    pub(super) async fn save_database_patch(
        &self,
        name: &str,
        patch: HashMap<String, ConfigValue>,
        expected: Option<&str>,
    ) -> Result<nuono_plugin::PluginConfigSnapshot> {
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| NuonoError::ConfigLoad(format!("配置 '{name}' 未注册")))?;
        self.save_database_patch_with_schema(name, patch, expected, &schema)
            .await
    }
}
