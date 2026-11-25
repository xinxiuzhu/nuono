//! 插件业务配置与服务级触发配置的持久化管理。
//!
//! 数据库只保存显式差异，未覆盖字段使用代码默认值；自动注入的 prefix、
//! 权限和名单字段随后由插件宿主读取，决定触发器暴露与调度。这里的 schema
//! 校验面向管理输入，插件仍应在产生外部副作用前校验业务参数和权限。

use super::plugin_files::{
    PluginConfigHandle, PluginConfigMigrationSource, PluginConfigStatus, PluginFiles, validate_name,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::debug;

use crate::config::schema::{ConfigSchema, ConfigValue};
#[cfg(test)]
use crate::core::config_schema::ConfigFieldUi;
use crate::core::plugin_service::Plugins;
use crate::error::{NuonoError, Result};

/// 管理表单的一致快照；值、声明与版本必须一起取得，不能分别读取后拼接。
pub struct PluginConfigManagementSnapshot {
    pub schema: ConfigSchema,
    pub values: HashMap<String, ConfigValue>,
    pub overridden_keys: Vec<String>,
    pub status: PluginConfigStatus,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PluginImportPreview {
    pub plugin_id: String,
    pub source_revision: String,
    pub database_revision: String,
    pub fields: Vec<PluginImportField>,
}
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PluginImportField {
    pub key: String,
    pub value: Option<serde_json::Value>,
    pub secret: bool,
    pub service: bool,
    pub conflict: bool,
    pub error: Option<String>,
}
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginImportChoice {
    Import,
    Inherit,
    KeepDatabase,
    Discard,
}
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct PluginImportRequest {
    pub source_revision: String,
    pub database_revision: String,
    pub fields: HashMap<String, PluginImportChoice>,
}

/// 以数据库为权威来源并维护不可变运行时快照的插件配置管理器；
/// 未挂载数据库的独立调用方仍可使用基于本地文件的嵌入模式。
#[derive(Debug)]
pub struct PluginConfigManager {
    schemas: Arc<RwLock<HashMap<String, ConfigSchema>>>,
    resources: RwLock<
        HashMap<String, Vec<nuono_app::configuration_resources::ConfigurationResourceDefinition>>,
    >,
    versions: RwLock<HashMap<String, i64>>,
    declared_versions: RwLock<HashMap<String, i64>>,
    import_secret_declarations: RwLock<HashMap<String, Vec<crate::plugin::PluginSecretField>>>,
    // 未完成的历史迁移只保留校验输入，不向表单或普通配置读写发布候选声明。
    migration_candidates: Mutex<HashMap<String, ConfigSchema>>,
    config_dir: PathBuf,
    database: Option<crate::db::Database>,
    files: PluginFiles,
    writes: Mutex<()>,
}

impl PluginConfigManager {
    pub fn new(config_dir: PathBuf) -> Self {
        Self {
            schemas: Arc::new(RwLock::new(HashMap::new())),
            resources: RwLock::new(HashMap::new()),
            versions: RwLock::new(HashMap::new()),
            declared_versions: RwLock::new(HashMap::new()),
            import_secret_declarations: RwLock::new(HashMap::new()),
            migration_candidates: Mutex::new(HashMap::new()),
            config_dir,
            database: None,
            files: PluginFiles::default(),
            writes: Mutex::new(()),
        }
    }

    /// 创建基于数据库的运行时插件配置管理器，由源码插件声明提供默认值。
    pub fn with_database(config_dir: PathBuf, database: crate::db::Database) -> Self {
        Self::with_legacy_database(config_dir, database)
    }

    pub fn with_legacy_database(config_dir: PathBuf, database: crate::db::Database) -> Self {
        Self {
            database: Some(database),
            ..Self::new(config_dir)
        }
    }

    pub async fn resource_definitions(
        &self,
        name: &str,
    ) -> Vec<nuono_app::configuration_resources::ConfigurationResourceDefinition> {
        self.resources
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default()
    }
    pub async fn register_resources(
        &self,
        name: &str,
        resources: Vec<nuono_app::configuration_resources::ConfigurationResourceDefinition>,
    ) -> Result<()> {
        let mut keys = std::collections::HashSet::new();
        for resource in &resources {
            resource.initial()?;
            if !keys.insert(&resource.key) {
                return Err(NuonoError::ConfigValidation("重复结构配置字段".into()));
            }
        }
        self.resources.write().await.insert(name.into(), resources);
        Ok(())
    }
    pub async fn resource_definition(
        &self,
        name: &str,
        key: &str,
    ) -> Result<nuono_app::configuration_resources::ConfigurationResourceDefinition> {
        self.resources
            .read()
            .await
            .get(name)
            .and_then(|resources| resources.iter().find(|r| r.key == key))
            .cloned()
            .ok_or_else(|| NuonoError::ConfigValidation(format!("未声明结构配置字段 {key}")))
    }
    async fn validate_resources(
        &self,
        name: &str,
        values: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<()> {
        for resource in self.resources.read().await.get(name).into_iter().flatten() {
            if let Some(raw) = values.get(&resource.key) {
                let raw = raw.as_str().ok_or_else(|| {
                    NuonoError::ConfigValidation("结构配置必须使用 JSON 文本".into())
                })?;
                resource.check(&serde_json::from_str(raw)?)?;
            }
        }
        Ok(())
    }

    pub fn plugin_dir(&self, name: &str) -> PathBuf {
        self.config_dir.join(name)
    }

    pub fn sv_config_path(&self, plugin_name: &str, sv_name: &str) -> PathBuf {
        self.plugin_dir(plugin_name)
            .join("sv")
            .join(format!("{}.toml", sanitize_component(sv_name)))
    }

    /// 注册当前配置声明；数据库模式要求先明确处理尚未导入的 TOML。
    pub async fn register(&self, name: &str, schema: ConfigSchema) -> Result<()> {
        self.register_with_service_config(name, schema, None).await
    }

    pub async fn register_with_service_config(
        &self,
        name: &str,
        schema: ConfigSchema,
        service_config: Option<&Plugins>,
    ) -> Result<()> {
        self.register_with_migration(name, schema, service_config, 1, None)
            .await
    }

    pub async fn register_with_migration(
        &self,
        name: &str,
        schema: ConfigSchema,
        service_config: Option<&Plugins>,
        version: i64,
        migration: Option<&dyn nuono_app::config_schema::ConfigurationMigration>,
    ) -> Result<()> {
        validate_name(name)?;
        for (key, field) in &schema {
            nuono_plugin::config_validation::validate_config_ui_credentials(key, field.ui())
                .map_err(NuonoError::ConfigValidation)?;
        }
        if schema.contains_key("auto_listen_enabled") {
            return Err(NuonoError::ConfigValidation(
                "auto_listen_enabled 是框架自动监听设置，不能声明为插件业务配置".into(),
            ));
        }
        let _write = self.writes.lock().await;
        let mut combined = service_schema(service_config);
        combined.extend(schema);
        let schema = combined;
        self.declared_versions
            .write()
            .await
            .insert(name.into(), version);
        self.migration_candidates.lock().await.remove(name);

        if let Some(db) = &self.database {
            if let Err(error) = self.ensure_database_source(name).await {
                self.migration_candidates
                    .lock()
                    .await
                    .insert(name.to_owned(), schema);
                self.files.record_error(name, error.to_string()).await;
                return Err(error);
            }
            let resources = self
                .resources
                .read()
                .await
                .get(name)
                .cloned()
                .unwrap_or_default();
            db.upgrade_plugin_configuration(
                name,
                version,
                &secret_fields(&schema),
                |from, target, values, service| {
                    use nuono_app::config_schema::ConfigurationTarget;
                    let converted = if from == version || values.is_empty() {
                        values.clone()
                    } else {
                        migration
                            .ok_or_else(|| {
                                NuonoError::ConfigValidation(
                                    "配置格式改变但插件未提供转换器".into(),
                                )
                            })?
                            .upgrade(from, version, target, values)
                            .map_err(NuonoError::ConfigValidation)?
                    };
                    for (key, raw) in converted.iter().chain(service) {
                        let field = schema.get(key).ok_or_else(|| {
                            NuonoError::ConfigValidation(format!("转换后配置字段 {key} 未声明"))
                        })?;
                        let permitted = match target {
                            ConfigurationTarget::Global => true,
                            ConfigurationTarget::User(_) => field.user_scoped(),
                            ConfigurationTarget::Group(_) => field.group_scoped(),
                        };
                        if !permitted {
                            return Err(NuonoError::ConfigValidation(format!(
                                "转换后字段 {key} 不允许用于该对象"
                            )));
                        }
                        let value: ConfigValue = serde_json::from_value(raw.clone())?;
                        validate_config_value(key, &value, field)
                            .map_err(NuonoError::ConfigValidation)?;
                    }
                    if target == ConfigurationTarget::Global {
                        for resource in &resources {
                            if let Some(raw) = converted.get(&resource.key) {
                                resource.check(&serde_json::from_str(
                                    raw.as_str().ok_or_else(|| {
                                        NuonoError::ConfigValidation(
                                            "结构配置必须使用JSON文本".into(),
                                        )
                                    })?,
                                )?)?;
                            }
                        }
                        database_values(
                            &schema,
                            &nuono_storage::plugins::PluginConfiguration {
                                values: converted.clone(),
                                service: service.clone(),
                                revision: 1,
                                config_version: version,
                            },
                        )?;
                    }
                    Ok(converted)
                },
            )
            .await?;
            self.versions.write().await.insert(name.into(), version);
            self.load_database(name, &schema).await?;
            self.schemas.write().await.insert(name.to_owned(), schema);
            return Ok(());
        }

        if let Err(error) = self.migrate_legacy_with_schema(name, None, &schema).await {
            self.migration_candidates
                .lock()
                .await
                .insert(name.to_string(), schema);
            self.files.record_error(name, error.to_string()).await;
            return Err(error);
        }
        if let Err(error) = self.ensure_config_file(name, &schema).await {
            self.files.record_error(name, error.to_string()).await;
            return Err(error);
        }

        // 文件和迁移通过候选声明校验后才发布；失败注册保留原有声明和有效快照。
        self.schemas.write().await.insert(name.to_string(), schema);
        debug!("已为插件 {} 初始化配置: {}/config.toml", name, name);
        Ok(())
    }

    pub async fn list_configs(&self) -> Vec<String> {
        self.schemas.read().await.keys().cloned().collect()
    }

    pub async fn get_schema(&self, name: &str) -> Option<ConfigSchema> {
        let schemas = self.schemas.read().await;
        schemas.get(name).cloned()
    }

    pub async fn ensure_service_fields(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let _write = self.writes.lock().await;
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        self.ensure_config_file(name, &schema).await
    }

    /// 表单仅由当前已注册的代码声明生成，用户文件不能定义表单或权限。
    pub async fn get_form_schema(&self, name: &str) -> Option<ConfigSchema> {
        self.get_schema(name).await
    }

    /// 读取当前配置值
    pub async fn get_values(&self, name: &str) -> Result<HashMap<String, ConfigValue>> {
        if !self.config_exists(name).await? && self.files.handle(name).await.is_none() {
            let schemas = self.schemas.read().await;
            let schema = schemas
                .get(name)
                .ok_or_else(|| NuonoError::ConfigLoad(format!("配置 '{}' 未注册", name)))?;
            return Ok(schema_to_defaults(schema));
        }

        let structured = self.read_structured(name).await?;
        Ok(structured
            .iter()
            .map(|(k, v)| (k.clone(), v.data_value()))
            .collect())
    }

    /// 返回同一轮读取的表单值和版本；坏文件仍展示最后有效值及对应错误状态。
    pub async fn management_snapshot(&self, name: &str) -> Result<PluginConfigManagementSnapshot> {
        validate_name(name)?;
        let _write = self.writes.lock().await;
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        let values = match self.read_structured_locked(name).await {
            Ok(structured) => structured
                .into_iter()
                .map(|(key, entry)| (key, entry.data_value()))
                .collect(),
            Err(error) => {
                self.files.record_error(name, error.to_string()).await;
                HashMap::new()
            }
        };
        let secret_keys: std::collections::HashSet<String> =
            secret_fields(&schema).into_iter().collect();
        let mut overridden_keys = if let Some(db) = self.database.as_ref() {
            match db.plugin_configuration(name).await {
                Ok(saved) => saved
                    .values
                    .keys()
                    .filter(|key| {
                        schema.contains_key(*key)
                            && !is_service_field(key)
                            && !secret_keys.contains(*key)
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
                Err(_) => Vec::new(),
            }
        } else {
            let defaults = schema_to_defaults(&schema);
            values
                .iter()
                .filter(|(key, value)| {
                    !is_service_field(key)
                        && !secret_keys.contains(*key)
                        && defaults.get(*key) != Some(*value)
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>()
        };
        overridden_keys.sort();
        let status = self.status(name).await;
        Ok(PluginConfigManagementSnapshot {
            schema,
            values,
            overridden_keys,
            status,
        })
    }

    /// 读取插件运行时使用的扁平 JSON 值。
    ///
    /// 数据库中的显式差异覆盖当前代码默认值，运行时只读取验证后的扁平值。
    pub async fn runtime_from_configuration(
        &self,
        name: &str,
        saved: &nuono_storage::plugins::PluginConfiguration,
    ) -> Result<serde_json::Value> {
        let schemas = self.schemas.read().await;
        let schema = schemas
            .get(name)
            .ok_or_else(|| NuonoError::PluginNotFound(name.into()))?;
        if self.versions.read().await.get(name).copied() != Some(saved.config_version) {
            return Err(NuonoError::ConfigValidation(
                "插件配置版本与当前代码不一致".into(),
            ));
        }
        let values = database_values(schema, saved)?
            .into_iter()
            .map(|(key, value)| serde_json::to_value(value.data_value()).map(|value| (key, value)))
            .collect::<std::result::Result<serde_json::Map<_, _>, _>>()?;
        Ok(values.into())
    }

    pub async fn get_runtime_config(&self, name: &str) -> Result<serde_json::Value> {
        self.read_structured(name).await?;
        Ok(self
            .files
            .runtime(name)
            .await
            .unwrap_or_else(|| serde_json::json!({})))
    }

    /// 将持久化的运行时值覆盖到插件默认配置上。
    pub async fn merge_runtime_config(
        &self,
        name: &str,
        defaults: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut merged = match defaults {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        if let serde_json::Value::Object(file_values) = self.get_runtime_config(name).await? {
            merged.extend(file_values);
        }
        Ok(serde_json::Value::Object(merged))
    }

    /// 保存配置值（只更新 data 字段，保留元数据）
    pub async fn set_values(&self, name: &str, values: HashMap<String, ConfigValue>) -> Result<()> {
        self.set_values_if_revision(name, values, None).await
    }

    pub async fn set_values_if_revision(
        &self,
        name: &str,
        values: HashMap<String, ConfigValue>,
        expected: Option<&str>,
    ) -> Result<()> {
        self.set_values_with_snapshot(name, values, expected)
            .await
            .map(|_| ())
    }

    /// 在同一个写锁下读取当前配置及匹配的修订版本；非法配置明确失败。
    pub async fn read_latest(&self, name: &str) -> Result<nuono_plugin::PluginConfigSnapshot> {
        validate_name(name)?;
        let _write = self.writes.lock().await;
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| NuonoError::ConfigLoad(format!("配置 '{name}' 未注册")))?;
        if self.database.is_some() {
            self.load_database(name, &schema).await?;
            return self
                .files
                .snapshot(name)
                .await
                .ok_or_else(|| NuonoError::ConfigLoad("插件配置快照不存在".into()));
        }
        self.migrate_legacy(name, None).await?;
        self.files
            .load(name, &self.config_path(name), &schema, true)
            .await?;
        self.files
            .snapshot(name)
            .await
            .ok_or_else(|| NuonoError::ConfigLoad("插件配置快照不存在".into()))
    }

    /// SDK 配置更新复用与管理面板相同的校验和写入路径，
    /// 但会校验期望版本号（CAS）并在释放锁前返回一致的写入后快照。
    pub async fn update_config_versioned(
        &self,
        name: &str,
        patch: serde_json::Value,
        expected: &str,
    ) -> Result<nuono_plugin::PluginConfigSnapshot> {
        if expected.trim().is_empty() {
            return Err(NuonoError::ConfigValidation(
                "插件配置保存需要版本号".into(),
            ));
        }
        let values: HashMap<String, ConfigValue> = serde_json::from_value(patch).map_err(|_| {
            NuonoError::ConfigValidation("插件配置补丁必须包含受支持的字段值".into())
        })?;
        self.set_values_with_snapshot(name, values, Some(expected))
            .await
    }

    /// 移除指定字段的显式覆盖值，使后续读取回退到当前代码声明的默认值。
    pub async fn reset_fields(&self, name: &str, fields: &[String], expected: &str) -> Result<()> {
        self.reset_fields_as(None, name, fields, expected).await
    }

    pub async fn reset_fields_as(
        &self,
        actor: Option<i64>,
        name: &str,
        fields: &[String],
        expected: &str,
    ) -> Result<()> {
        validate_name(name)?;
        let _write = self.writes.lock().await;
        let db = self
            .database
            .as_ref()
            .ok_or_else(|| NuonoError::NotImplemented("恢复继承需要数据库配置存储".into()))?;
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| NuonoError::ConfigLoad(format!("配置 '{name}' 未注册")))?;
        let mut saved = db.plugin_configuration(name).await?;
        if expected != saved.revision.to_string() {
            return Err(NuonoError::ConfigValidation(
                "插件配置版本冲突，请刷新后重试".into(),
            ));
        }
        for field in fields {
            if !schema.contains_key(field) {
                return Err(NuonoError::ConfigValidation(format!(
                    "未声明的插件配置字段 {field}"
                )));
            }
            if let Ok(resource) = self.resource_definition(name, field).await {
                let current = match saved.values.get(field) {
                    Some(raw) => serde_json::from_str(raw.as_str().ok_or_else(|| {
                        NuonoError::ConfigValidation("结构配置必须使用JSON文本".into())
                    })?)?,
                    None => resource.initial()?,
                };
                resource.check(&current)?;
                let next = resource
                    .validated_write(name, current.revision, resource.default.clone())?
                    .next()?;
                saved.values.insert(
                    field.clone(),
                    serde_json::Value::String(serde_json::to_string(&next)?),
                );
            } else {
                saved.values.remove(field);
                saved.service.remove(field);
            }
        }
        let structured = database_values(&schema, &saved)?;
        let runtime = structured
            .iter()
            .map(|(key, entry)| {
                serde_json::to_value(entry.data_value()).map(|value| (key.clone(), value))
            })
            .collect::<std::result::Result<serde_json::Map<_, _>, _>>()?;
        let revision = db
            .save_plugin_configuration_as(actor, name, &saved, &secret_fields(&schema))
            .await?
            .to_string();
        self.files
            .publish_database(name, structured, runtime.into(), revision)
            .await;
        Ok(())
    }

    async fn set_values_with_snapshot(
        &self,
        name: &str,
        values: HashMap<String, ConfigValue>,
        expected: Option<&str>,
    ) -> Result<nuono_plugin::PluginConfigSnapshot> {
        validate_name(name)?;
        let _write = self.writes.lock().await;
        if self.database.is_some() {
            return self.save_database_patch(name, values, expected).await;
        }
        self.migrate_legacy(name, None).await?;
        let before = self.status(name).await.revision;
        if expected.is_some() && expected != before.as_deref() {
            return Err(NuonoError::ConfigValidation(
                "插件配置版本冲突，请刷新后重试".into(),
            ));
        }

        if !self.config_exists(name).await? {
            let schema = {
                let schemas = self.schemas.read().await;
                schemas
                    .get(name)
                    .cloned()
                    .ok_or_else(|| NuonoError::ConfigLoad(format!("配置 '{}' 未注册", name)))?
            };
            self.ensure_config_file(name, &schema).await?;
        }

        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        let mut structured = self
            .files
            .load(name, &self.config_path(name), &schema, true)
            .await?;
        if expected.is_some() && expected != self.status(name).await.revision.as_deref() {
            return Err(NuonoError::ConfigValidation(
                "插件配置已被手工编辑，请刷新后重试".into(),
            ));
        }
        for (key, value) in values {
            let entry = structured
                .get_mut(&key)
                .ok_or_else(|| NuonoError::ConfigValidation(format!("未知插件配置字段 {key}")))?;
            let field = schema.get(&key).ok_or_else(|| {
                NuonoError::ConfigValidation(format!("未声明的插件配置字段 {key}"))
            })?;
            validate_config_value(&key, &value, field).map_err(NuonoError::ConfigValidation)?;
            set_entry_data(&key, entry, value)?;
        }

        self.write_structured(name, &structured).await?;
        self.files
            .snapshot(name)
            .await
            .ok_or_else(|| NuonoError::ConfigLoad("插件配置快照不存在".into()))
    }

    fn config_path(&self, name: &str) -> PathBuf {
        self.config_dir.join(name).join("config.toml")
    }

    pub async fn status(&self, name: &str) -> PluginConfigStatus {
        let mut status = self.files.status(name, self.config_path(name)).await;
        if self.database.is_some() {
            status.source = "database";
            if !status.migration_conflict {
                status.path.clear();
            }
        }
        status
    }

    pub async fn config_handle(&self, name: &str) -> Option<PluginConfigHandle> {
        self.files.handle(name).await
    }

    pub async fn set_runtime_error(&self, name: &str, error: Option<String>) {
        self.files.runtime_error(name, error).await;
    }

    pub async fn refresh(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let _write = self.writes.lock().await;
        if self.migration_candidates.lock().await.contains_key(name) {
            return Err(NuonoError::ConfigValidation(
                "插件配置迁移冲突：请先明确选择导入 TOML 或使用数据库配置".into(),
            ));
        }
        self.migrate_legacy(name, None).await?;
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        if self.database.is_some() {
            self.load_database(name, &schema).await?;
            return Ok(());
        }
        self.files
            .load(name, &self.config_path(name), &schema, true)
            .await?;
        Ok(())
    }

    pub async fn set_values_as(
        &self,
        actor: i64,
        name: &str,
        values: HashMap<String, ConfigValue>,
        expected: &str,
    ) -> Result<()> {
        let _write = self.writes.lock().await;
        let schema = self
            .schemas
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| NuonoError::PluginNotFound(name.into()))?;
        self.save_database_patch_with_schema_as(Some(actor), name, values, Some(expected), &schema)
            .await?;
        Ok(())
    }

    async fn save_database_patch_with_schema(
        &self,
        name: &str,
        patch: HashMap<String, ConfigValue>,
        expected: Option<&str>,
        schema: &ConfigSchema,
    ) -> Result<nuono_plugin::PluginConfigSnapshot> {
        self.save_database_patch_with_schema_as(None, name, patch, expected, schema)
            .await
    }

    async fn save_database_patch_with_schema_as(
        &self,
        actor: Option<i64>,
        name: &str,
        patch: HashMap<String, ConfigValue>,
        expected: Option<&str>,
        schema: &ConfigSchema,
    ) -> Result<nuono_plugin::PluginConfigSnapshot> {
        let db = self
            .database
            .as_ref()
            .ok_or_else(|| NuonoError::ConfigLoad("未连接插件配置数据库".into()))?;
        let mut saved = db.plugin_configuration(name).await?;
        if expected.is_some_and(|value| value != saved.revision.to_string()) {
            return Err(NuonoError::ConfigValidation(
                "插件配置版本冲突，请刷新后重试".into(),
            ));
        }
        let service_fields = service_schema(None);
        for (key, mut value) in patch {
            let field = schema.get(&key).ok_or_else(|| {
                NuonoError::ConfigValidation(format!("未声明的插件配置字段 {key}"))
            })?;
            validate_config_value(&key, &value, field).map_err(NuonoError::ConfigValidation)?;
            if let Ok(resource) = self.resource_definition(name, &key).await {
                let raw = serde_json::to_value(&value)?;
                let submitted: nuono_app::configuration_resources::ConfigurationResource =
                    serde_json::from_str(raw.as_str().ok_or_else(|| {
                        NuonoError::ConfigValidation("结构配置必须使用JSON文本".into())
                    })?)?;
                resource.check(&submitted)?;
                let current = match saved.values.get(&key) {
                    Some(raw) => serde_json::from_str(raw.as_str().ok_or_else(|| {
                        NuonoError::ConfigValidation("结构配置必须使用JSON文本".into())
                    })?)?,
                    None => resource.initial()?,
                };
                resource.check(&current)?;
                if submitted.revision != current.revision {
                    return Err(NuonoError::Conflict("配置资源已变化，请刷新后重试".into()));
                }
                let next = resource
                    .validated_write(name, current.revision, submitted.value)?
                    .next()?;
                value = serde_json::from_value(serde_json::Value::String(serde_json::to_string(
                    &next,
                )?))?;
            }
            let target = if service_fields.contains_key(&key) || key == "auto_listen_enabled" {
                &mut saved.service
            } else {
                &mut saved.values
            };
            target.insert(key, serde_json::to_value(value)?);
        }
        let structured = database_values(schema, &saved)?;
        let runtime = structured
            .iter()
            .map(|(key, entry)| {
                serde_json::to_value(entry.data_value()).map(|value| (key.clone(), value))
            })
            .collect::<std::result::Result<serde_json::Map<_, _>, _>>()?;
        let revision = db
            .save_plugin_configuration_as(actor, name, &saved, &secret_fields(schema))
            .await?
            .to_string();
        self.files
            .publish_database(name, structured, runtime.clone().into(), revision.clone())
            .await;
        Ok(nuono_plugin::PluginConfigSnapshot {
            values: runtime.into(),
            revision,
        })
    }
}

mod import;
mod migration;
mod schema_projection;

pub(crate) use schema_projection::set_entry_data;
use schema_projection::{
    database_values, sanitize_component, schema_to_defaults, service_schema, toml_contains_datetime,
};
pub use schema_projection::{
    is_service_field, plugin_fields_to_schema, secret_fields, validate_config_value,
};

#[cfg(test)]
#[path = "tests.rs"]
mod test_suite;
