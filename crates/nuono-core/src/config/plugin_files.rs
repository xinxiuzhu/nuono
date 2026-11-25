//! 插件配置内存快照管理，以及无数据库独立嵌入模式下的文件后端适配器。
use super::schema::{ConfigSchema, ConfigValue, StructuredConfig, StructuredConfigField};
use crate::error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tokio::sync::Mutex;

pub use nuono_plugin::config::PluginConfigHandle;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PluginConfigStatus {
    pub source: &'static str,
    pub path: String,
    pub revision: Option<String>,
    pub error: Option<String>,
    pub runtime_error: Option<String>,
    pub migration_conflict: bool,
}
#[derive(Clone, Copy, Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginConfigMigrationSource {
    File,
    Database,
}

#[derive(Debug)]
struct FileState {
    document: toml::Table,
    structured: StructuredConfig,
    handle: PluginConfigHandle,
    revision: String,
    error: Option<String>,
    runtime_error: Option<String>,
}
#[derive(Debug, Default)]
pub(super) struct PluginFiles {
    states: Mutex<HashMap<String, FileState>>,
    errors: Mutex<HashMap<String, String>>,
}

pub(super) fn validate_name(name: &str) -> Result<()> {
    if crate::core::Paths::is_reserved_plugin_name(name) {
        return Err(NuonoError::ConfigValidation(
            "hub 是管理界面的保留目录名称，插件需要使用其他名称".into(),
        ));
    }
    let path = Path::new(name);
    if name.is_empty()
        || name.contains(['/', '\\'])
        || path.components().count() != 1
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(NuonoError::ConfigValidation(
            "插件名必须是单个安全目录名称".into(),
        ));
    }
    Ok(())
}
pub(super) fn revision(raw: &[u8]) -> String {
    hex::encode(Sha256::digest(raw))
}

pub(super) fn parse(
    raw: &str,
    schema: &ConfigSchema,
) -> Result<(toml::Table, StructuredConfig, Value)> {
    let document: toml::Table = toml::from_str(raw)
        .map_err(|_| NuonoError::ConfigLoad("插件 TOML 解析失败，请检查文件语法".into()))?;
    let mut structured = StructuredConfig::new();
    let mut runtime = serde_json::Map::new();
    for (key, value) in &document {
        let json =
            serde_json::to_value(value).map_err(|e| NuonoError::Serialization(e.to_string()))?;
        let wrapped = value
            .as_table()
            .is_some_and(|v| v.contains_key("type") && v.contains_key("data"));
        let entry = if wrapped {
            Some(
                serde_json::from_value::<StructuredConfigField>(json.clone()).map_err(|_| {
                    NuonoError::ConfigValidation(format!("插件配置字段 {key} 的结构或类型错误"))
                })?,
            )
        } else if let Ok(data) = serde_json::from_value::<ConfigValue>(json.clone()) {
            let field = schema
                .get(key)
                .cloned()
                .unwrap_or_else(|| infer_field(key, &data));
            let mut entry = StructuredConfigField::from_config_field(&field);
            super::plugin_config::set_entry_data(key, &mut entry, data)?;
            Some(entry)
        } else {
            None
        };
        let actual = if wrapped {
            json.get("data").cloned().unwrap_or(Value::Null)
        } else {
            json
        };
        if let Some(field) = schema.get(key) {
            let data: ConfigValue = serde_json::from_value(actual.clone()).map_err(|_| {
                NuonoError::ConfigValidation(format!("插件配置字段 {key} 类型错误"))
            })?;
            super::plugin_config::validate_config_value(key, &data, field)
                .map_err(NuonoError::ConfigValidation)?;
        }
        validate_service_value(key, &actual)?;
        runtime.insert(key.clone(), actual);
        if let Some(entry) = entry {
            structured.insert(key.clone(), entry);
        }
    }
    Ok((document, structured, Value::Object(runtime)))
}
fn validate_service_value(key: &str, value: &Value) -> Result<()> {
    let valid = match key {
        "pm" => value.as_i64().is_some_and(|v| (0..=6).contains(&v)),
        "area" => value
            .as_str()
            .is_some_and(|v| matches!(v, "ALL" | "GROUP" | "DIRECT" | "FOLLOW_PLUGIN")),
        "prefix" | "force_prefix" | "alias" | "black_list" | "white_list" | "disabled_commands" => {
            value
                .as_array()
                .is_some_and(|v| v.iter().all(Value::is_string))
        }
        "allow_empty_prefix" | "disable_force_prefix" => value.is_boolean(),
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(NuonoError::ConfigValidation(format!(
            "插件服务字段 {key} 的类型或范围错误"
        )))
    }
}

fn infer_field(key: &str, data: &ConfigValue) -> super::schema::ConfigField {
    use super::schema::ConfigField;
    let mut value = serde_json::json!({"title":key,"desc":"","default":data});
    value["type"] = Value::String(
        match data {
            ConfigValue::String(_) => "string",
            ConfigValue::Bool(_) => "bool",
            ConfigValue::Int(_) => "int",
            ConfigValue::Float(_) => "float",
            ConfigValue::ListStr(_) => "list_str",
        }
        .into(),
    );
    if matches!(data, ConfigValue::ListStr(_)) {
        value["options"] = serde_json::json!([]);
    }
    serde_json::from_value::<ConfigField>(value).expect("inferred config field is valid")
}

impl PluginFiles {
    /// 直接将已校验的数据库配置快照发布到内存，不读写磁盘文件。
    pub async fn publish_database(
        &self,
        name: &str,
        structured: StructuredConfig,
        runtime: Value,
        revision: String,
    ) {
        let mut states = self.states.lock().await;
        if let Some(state) = states.get_mut(name) {
            if state.revision != revision || state.handle.global_snapshot().as_ref() != &runtime {
                state.handle.publish(runtime);
                state.runtime_error = None;
            }
            state.structured = structured;
            state.revision = revision;
            state.error = None;
        } else {
            states.insert(
                name.into(),
                FileState {
                    document: toml::Table::new(),
                    structured,
                    handle: PluginConfigHandle::for_plugin(name, runtime),
                    revision,
                    error: None,
                    runtime_error: None,
                },
            );
        }
        self.errors.lock().await.remove(name);
    }
    pub async fn load(
        &self,
        name: &str,
        path: &Path,
        schema: &ConfigSchema,
        strict: bool,
    ) -> Result<StructuredConfig> {
        let result: Result<StructuredConfig> = async {
            let raw = tokio::fs::read(path)
                .await
                .map_err(|e| NuonoError::FileRead {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?;
            let hash = revision(&raw);
            let mut states = self.states.lock().await;
            // 即使字节未变，重新校验新声明，避免升级后继续使用旧约束。
            let text = std::str::from_utf8(&raw)
                .map_err(|_| NuonoError::ConfigLoad("插件配置必须为 UTF-8".into()))?;
            let (document, structured, runtime) = parse(text, schema)?;
            if let Some(state) = states.get_mut(name) {
                // 已发布的必填字段不能通过手删绕过验证。尚未出现的新字段留给
                // register 的补默认流程，候选完整文档会在 write 中再次校验。
                for (key, field) in schema {
                    if field.ui().required
                        && state.document.contains_key(key)
                        && !document.contains_key(key)
                    {
                        return Err(NuonoError::ConfigValidation(format!(
                            "插件配置字段 {key} 为必填项，不能删除"
                        )));
                    }
                }
                if state.revision != hash {
                    // 发布不可变快照；派生缓存由消费者比较快照身份判断是否过期。
                    // 文件层不依赖群配置服务，也不要求调用方补发失效通知。
                    state.handle.publish(runtime);
                    state.runtime_error = None;
                }
                state.document = document;
                state.structured = structured.clone();
                state.revision = hash;
                state.error = None;
            } else {
                states.insert(
                    name.into(),
                    FileState {
                        document,
                        structured: structured.clone(),
                        handle: PluginConfigHandle::for_plugin(name, runtime),
                        revision: hash,
                        error: None,
                        runtime_error: None,
                    },
                );
            }
            self.errors.lock().await.remove(name);
            Ok(structured)
        }
        .await;
        if let Err(error) = &result {
            let message = error.to_string();
            self.errors
                .lock()
                .await
                .insert(name.into(), message.clone());
            let mut states = self.states.lock().await;
            if let Some(state) = states.get_mut(name) {
                state.error = Some(message);
                if !strict {
                    return Ok(state.structured.clone());
                }
            }
        }
        result
    }
    pub async fn runtime(&self, name: &str) -> Option<Value> {
        self.states
            .lock()
            .await
            .get(name)
            .map(|s| s.handle.global_snapshot().as_ref().clone())
    }
    pub async fn handle(&self, name: &str) -> Option<PluginConfigHandle> {
        self.states.lock().await.get(name).map(|s| s.handle.clone())
    }
    pub async fn snapshot(&self, name: &str) -> Option<nuono_plugin::PluginConfigSnapshot> {
        self.states
            .lock()
            .await
            .get(name)
            .map(|state| nuono_plugin::PluginConfigSnapshot {
                values: state.handle.global_snapshot().as_ref().clone(),
                revision: state.revision.clone(),
            })
    }
    pub async fn status(&self, name: &str, path: PathBuf) -> PluginConfigStatus {
        let states = self.states.lock().await;
        let state = states.get(name);
        let error = self.errors.lock().await.get(name).cloned();
        PluginConfigStatus {
            source: "file",
            path: path.display().to_string(),
            revision: state.map(|s| s.revision.clone()),
            migration_conflict: error.as_ref().is_some_and(|e| e.contains("迁移冲突")),
            error,
            runtime_error: state.and_then(|s| s.runtime_error.clone()),
        }
    }
    pub async fn record_error(&self, name: &str, message: String) {
        self.errors.lock().await.insert(name.into(), message);
    }
    pub async fn runtime_error(&self, name: &str, error: Option<String>) {
        if let Some(state) = self.states.lock().await.get_mut(name) {
            state.runtime_error = error;
        }
    }
    pub async fn write(
        &self,
        name: &str,
        path: &Path,
        config: &StructuredConfig,
        schema: &ConfigSchema,
    ) -> Result<()> {
        let (mut document, expected) = {
            let states = self.states.lock().await;
            match states.get(name) {
                Some(s) => (s.document.clone(), Some(s.revision.clone())),
                None => (toml::Table::new(), None),
            }
        };
        for (key, entry) in config {
            let flat = document.get(key).is_some_and(|v| {
                !v.as_table()
                    .is_some_and(|v| v.contains_key("type") && v.contains_key("data"))
            });
            let value = if flat {
                toml::Value::try_from(entry.data_value())
            } else {
                toml::Value::try_from(entry)
            }
            .map_err(|e| NuonoError::Serialization(e.to_string()))?;
            document.insert(key.clone(), value);
        }
        let raw = toml::to_string_pretty(&document)
            .map_err(|e| NuonoError::Serialization(e.to_string()))?;
        parse(&raw, schema)?;
        atomic_write(path, raw.as_bytes(), expected.as_deref()).await?;
        self.load(name, path, schema, true).await?;
        Ok(())
    }
}

/// 原子替换保护读者；版本核对检测已发生的外部编辑，不声称锁住手工编辑器。
pub(super) async fn atomic_write(path: &Path, raw: &[u8], expected: Option<&str>) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| NuonoError::ConfigLoad("配置目录无效".into()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| NuonoError::FileWrite {
            path: parent.display().to_string(),
            reason: e.to_string(),
        })?;
    let temporary = parent.join(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .await
            .map_err(|e| NuonoError::FileWrite {
                path: temporary.display().to_string(),
                reason: e.to_string(),
            })?;
        file.write_all(raw)
            .await
            .map_err(|e| NuonoError::FileWrite {
                path: temporary.display().to_string(),
                reason: e.to_string(),
            })?;
        file.sync_all().await.map_err(|e| NuonoError::FileWrite {
            path: temporary.display().to_string(),
            reason: e.to_string(),
        })?;
        drop(file);
        match tokio::fs::read(path).await {
            Ok(current) if expected != Some(revision(&current).as_str()) => {
                return Err(NuonoError::ConfigValidation(
                    "插件配置已被其他编辑修改，请刷新后重试".into(),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && expected.is_none() => {}
            Err(e) => {
                return Err(NuonoError::FileRead {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                });
            }
            _ => {}
        }
        // 首次创建通过硬链接的 create-if-absent 语义，避免覆盖并发创建的配置。
        if expected.is_none() {
            tokio::fs::hard_link(&temporary, path)
                .await
                .map_err(|e| NuonoError::FileWrite {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?;
        } else {
            tokio::fs::rename(&temporary, path)
                .await
                .map_err(|e| NuonoError::FileWrite {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?;
        }
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(temporary).await;
    result
}
