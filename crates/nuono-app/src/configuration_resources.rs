//! 带独立乐观锁版本号（`revision`）与插件自定义校验器的结构化全局配置资源。
use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationResource {
    pub revision: i64,
    pub version: i64,
    pub value: Value,
}
#[derive(Clone, Debug)]
pub struct ConfigurationResourceDefinition {
    pub key: String,
    pub label: String,
    pub description: String,
    pub version: i64,
    pub default: Value,
    pub max_bytes: usize,
    pub validator: fn(&Value) -> std::result::Result<(), String>,
}
impl ConfigurationResourceDefinition {
    pub fn validate(&self, value: &Value) -> Result<()> {
        if self.version < 1
            || self.key.is_empty()
            || self.key.len() > 128
            || !self
                .key
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            || self.max_bytes == 0
            || self.max_bytes > 16 * 1024 * 1024
        {
            return Err(NuonoError::ConfigValidation("配置资源声明无效".into()));
        }
        if serde_json::to_vec(value)?.len() > self.max_bytes {
            return Err(NuonoError::ConfigValidation(format!(
                "配置资源 {} 超出大小限制",
                self.key
            )));
        }
        std::panic::catch_unwind(|| (self.validator)(value))
            .map_err(|_| {
                NuonoError::ConfigValidation(format!("配置资源 {} 的校验器异常", self.key))
            })?
            .map_err(NuonoError::ConfigValidation)
    }
    pub fn initial(&self) -> Result<ConfigurationResource> {
        self.validate(&self.default)?;
        Ok(ConfigurationResource {
            revision: 0,
            version: self.version,
            value: self.default.clone(),
        })
    }
    pub fn check(&self, resource: &ConfigurationResource) -> Result<()> {
        if resource.revision < 0 || resource.version != self.version {
            return Err(NuonoError::ConfigValidation(format!(
                "配置资源 {} 的版本不受支持",
                self.key
            )));
        }
        self.validate(&resource.value)
    }
    pub fn field(&self) -> Result<crate::config_schema::PluginConfigField> {
        Ok(crate::config_schema::PluginConfigField::textarea(
            &self.key,
            &self.label,
            &self.description,
            &serde_json::to_string(&self.initial()?)?,
        ))
    }
    /// 宿主在将写入请求交给存储层落盘前，先使用已安装插件注册的资源声明完成校验并封装为已校验对象（存储层仅作透传落盘）。
    pub fn validated_write(
        &self,
        plugin: &str,
        expected_revision: i64,
        value: Value,
    ) -> Result<ValidatedConfigurationResource> {
        self.validate(&value)?;
        if expected_revision < 0 {
            return Err(NuonoError::ConfigValidation("配置资源版本无效".into()));
        }
        Ok(ValidatedConfigurationResource {
            plugin_id: plugin.into(),
            definition: self.clone(),
            expected_revision,
            value,
        })
    }
}
#[derive(Clone, Debug)]
pub struct ValidatedConfigurationResource {
    plugin_id: String,
    definition: ConfigurationResourceDefinition,
    expected_revision: i64,
    value: Value,
}
impl ValidatedConfigurationResource {
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
    pub fn key(&self) -> &str {
        &self.definition.key
    }
    pub fn expected_revision(&self) -> i64 {
        self.expected_revision
    }
    pub fn initial(&self) -> Result<ConfigurationResource> {
        self.definition.initial()
    }
    pub fn validate_current(&self, resource: &ConfigurationResource) -> Result<()> {
        self.definition.check(resource)
    }
    pub fn next(&self) -> Result<ConfigurationResource> {
        self.definition.validate(&self.value)?;
        Ok(ConfigurationResource {
            revision: self
                .expected_revision
                .checked_add(1)
                .ok_or_else(|| NuonoError::Conflict("配置资源版本已耗尽".into()))?,
            version: self.definition.version,
            value: self.value.clone(),
        })
    }
}
