//! 旧版 TOML 插件配置文件的预览与选择性导入流程（与数据库权威读写隔离）。

use std::collections::HashMap;

use super::{
    PluginConfigManager, PluginImportChoice, PluginImportField, PluginImportPreview,
    PluginImportRequest, database_values, is_service_field, plugin_fields_to_schema, secret_fields,
    toml_contains_datetime, validate_config_value, validate_name,
};
use crate::config::plugin_files;
use crate::config::schema::{ConfigSchema, ConfigValue};
use crate::core::config_schema::PluginConfigField;
use crate::error::{NuonoError, Result};

impl PluginConfigManager {
    pub async fn register_import_secrets(
        &self,
        name: &str,
        fields: Vec<crate::plugin::PluginSecretField>,
    ) {
        self.import_secret_declarations
            .write()
            .await
            .insert(name.into(), fields);
    }

    async fn import_schema(&self, name: &str) -> Result<ConfigSchema> {
        if let Some(schema) = self.migration_candidates.lock().await.get(name).cloned() {
            return Ok(schema);
        }
        self.schemas
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| NuonoError::PluginNotFound(name.into()))
    }

    pub async fn preview_import(&self, name: &str) -> Result<PluginImportPreview> {
        validate_name(name)?;
        let mut schema = self.import_schema(name).await?;
        let declared = self
            .import_secret_declarations
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        for field in &declared {
            if !schema.contains_key(&field.key) {
                schema.extend(plugin_fields_to_schema(&[PluginConfigField::password(
                    &field.key,
                    &field.title,
                    &field.description,
                )])?);
            }
        }
        let raw = tokio::fs::read(self.config_path(name))
            .await
            .map_err(|error| NuonoError::ConfigLoad(error.to_string()))?;
        let text = std::str::from_utf8(&raw)
            .map_err(|_| NuonoError::ConfigLoad("TOML 必须为 UTF-8".into()))?;
        let table: toml::Table =
            toml::from_str(text).map_err(|_| NuonoError::ConfigLoad("TOML 语法无效".into()))?;
        let db = self
            .database
            .as_ref()
            .ok_or_else(|| NuonoError::ConfigLoad("未连接配置数据库".into()))?;
        let saved = db.plugin_configuration(name).await?;
        let mut fields = Vec::new();
        let secrets = secret_fields(&schema);
        for (key, value) in table {
            let actual = if let toml::Value::Table(entry) = &value {
                entry.get("data").unwrap_or(&value)
            } else {
                &value
            };
            let typed = if toml_contains_datetime(actual) {
                Err("日期类型没有对应配置声明".to_owned())
            } else {
                serde_json::to_value(actual)
                    .map_err(|_| "不支持的 TOML 值".to_owned())
                    .and_then(|raw| {
                        serde_json::from_value::<ConfigValue>(raw)
                            .map_err(|_| "不支持的配置类型".to_owned())
                    })
                    .and_then(|typed| match schema.get(&key) {
                        Some(field) => validate_config_value(&key, &typed, field).map(|()| typed),
                        None => Err("字段未在当前插件中声明".to_owned()),
                    })
            };
            let secret = secrets.contains(&key);
            let service = is_service_field(&key);
            let secret_conflict = declared.iter().any(|field| field.key == key)
                && db
                    .get_secret(&format!("plugin.{name}.{key}"))
                    .await?
                    .is_some();
            fields.push(PluginImportField {
                key: key.clone(),
                value: if secret {
                    None
                } else {
                    typed
                        .as_ref()
                        .ok()
                        .and_then(|value| serde_json::to_value(value).ok())
                },
                secret,
                service,
                conflict: secret_conflict
                    || saved.values.contains_key(&key)
                    || saved.service.contains_key(&key),
                error: typed.err(),
            });
        }
        Ok(PluginImportPreview {
            plugin_id: name.into(),
            source_revision: plugin_files::revision(&raw),
            database_revision: saved.revision.to_string(),
            fields,
        })
    }

    pub async fn import_selected(
        &self,
        actor: i64,
        name: &str,
        request: PluginImportRequest,
    ) -> Result<()> {
        validate_name(name)?;
        let _write = self.writes.lock().await;
        let preview = self.preview_import(name).await?;
        if preview.source_revision != request.source_revision
            || preview.database_revision != request.database_revision
        {
            return Err(NuonoError::Conflict(
                "导入文件或数据库已改变，请重新预览".into(),
            ));
        }
        let schema = self.import_schema(name).await?;
        let raw = tokio::fs::read_to_string(self.config_path(name))
            .await
            .map_err(|error| NuonoError::ConfigLoad(error.to_string()))?;
        if plugin_files::revision(raw.as_bytes()) != request.source_revision {
            return Err(NuonoError::Conflict("导入文件已改变".into()));
        }
        let table: toml::Table =
            toml::from_str(&raw).map_err(|_| NuonoError::ConfigLoad("TOML 语法无效".into()))?;
        let db = self
            .database
            .as_ref()
            .ok_or_else(|| NuonoError::ConfigLoad("未连接配置数据库".into()))?;
        let mut saved = db.plugin_configuration(name).await?;
        if saved.revision.to_string() != request.database_revision {
            return Err(NuonoError::Conflict("数据库配置已改变".into()));
        }
        let previous_resource_values = saved.values.clone();
        if request.fields.len() != preview.fields.len() {
            return Err(NuonoError::ConfigValidation(
                "每个导入字段都必须明确选择".into(),
            ));
        }
        let declared = self
            .import_secret_declarations
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default();
        let mut imported_secrets = HashMap::new();
        for field in preview.fields {
            let choice = request.fields.get(&field.key).ok_or_else(|| {
                NuonoError::ConfigValidation(format!("字段 {} 未选择导入方式", field.key))
            })?;
            if declared.iter().any(|secret| secret.key == field.key)
                && !schema.contains_key(&field.key)
            {
                match choice {
                    PluginImportChoice::Import => {
                        if let Some(error) = field.error {
                            return Err(NuonoError::ConfigValidation(error));
                        }
                        let value = &table[&field.key];
                        let value = value
                            .as_table()
                            .and_then(|table| table.get("data"))
                            .unwrap_or(value);
                        let secret = value.as_str().ok_or_else(|| {
                            NuonoError::ConfigValidation("密钥必须是字符串".into())
                        })?;
                        imported_secrets.insert(field.key, Some(secret.to_owned()));
                    }
                    PluginImportChoice::Inherit => {
                        imported_secrets.insert(field.key, None);
                    }
                    _ => {}
                }
                continue;
            }
            match choice {
                PluginImportChoice::Discard => {}
                PluginImportChoice::KeepDatabase => {}
                PluginImportChoice::Inherit => {
                    if !schema.contains_key(&field.key) {
                        return Err(NuonoError::ConfigValidation("未知字段只能明确舍弃".into()));
                    }
                    saved.values.remove(&field.key);
                    saved.service.remove(&field.key);
                }
                PluginImportChoice::Import => {
                    if let Some(error) = field.error {
                        return Err(NuonoError::ConfigValidation(format!(
                            "字段 {}: {error}",
                            field.key
                        )));
                    }
                    let value = &table[&field.key];
                    let actual = if let toml::Value::Table(entry) = value {
                        entry.get("data").unwrap_or(value)
                    } else {
                        value
                    };
                    let target = if field.service {
                        &mut saved.service
                    } else {
                        &mut saved.values
                    };
                    target.insert(field.key, serde_json::to_value(actual)?);
                }
            }
        }
        for resource in self.resources.read().await.get(name).into_iter().flatten() {
            if previous_resource_values.get(&resource.key) != saved.values.get(&resource.key) {
                let parse = |raw: &serde_json::Value| -> Result<
                    nuono_app::configuration_resources::ConfigurationResource,
                > {
                    Ok(serde_json::from_str(raw.as_str().ok_or_else(|| {
                        NuonoError::ConfigValidation("结构配置必须使用JSON文本".into())
                    })?)?)
                };
                let previous = previous_resource_values
                    .get(&resource.key)
                    .map(parse)
                    .transpose()?
                    .unwrap_or(resource.initial()?);
                let imported = saved
                    .values
                    .get(&resource.key)
                    .map(parse)
                    .transpose()?
                    .unwrap_or(resource.initial()?);
                resource.check(&previous)?;
                resource.check(&imported)?;
                let next = resource
                    .validated_write(name, previous.revision, imported.value)?
                    .next()?;
                saved.values.insert(
                    resource.key.clone(),
                    serde_json::Value::String(serde_json::to_string(&next)?),
                );
            }
        }
        self.validate_resources(name, &saved.values).await?;
        let declared_version = self
            .declared_versions
            .read()
            .await
            .get(name)
            .copied()
            .ok_or_else(|| NuonoError::ConfigValidation("插件代码尚未声明配置版本".into()))?;
        saved.config_version = declared_version;
        database_values(&schema, &saved)?;
        db.save_plugin_configuration_import_at_version(
            Some(actor),
            name,
            &saved,
            &secret_fields(&schema),
            &imported_secrets,
            Some(declared_version),
        )
        .await?;
        self.schemas
            .write()
            .await
            .insert(name.into(), schema.clone());
        self.versions
            .write()
            .await
            .insert(name.into(), saved.config_version);
        self.migration_candidates.lock().await.remove(name);
        self.load_database(name, &schema).await?;
        let archive = self
            .config_path(name)
            .with_file_name(format!("config.imported-{}.toml", request.source_revision));
        tokio::fs::rename(self.config_path(name), archive)
            .await
            .map_err(|error| {
                NuonoError::ConfigLoad(format!("导入已提交，但归档源文件失败：{error}"))
            })?;
        Ok(())
    }
}
