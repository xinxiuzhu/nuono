//! 绑定到当前已安装插件作用域的全局结构化配置资源读写接口。
use super::{Database, Transaction};
use nuono_app::configuration_resources::ConfigurationResource;
use nuono_error::{NuonoError, Result};
impl Database {
    pub async fn read_configuration_resource(&self, key: &str) -> Result<ConfigurationResource> {
        self.configuration
            .as_ref()
            .ok_or_else(|| NuonoError::NotImplemented("数据库未连接插件配置能力".into()))?
            .configuration_resource_snapshot(key)
            .await
    }
}
impl Transaction {
    pub async fn read_configuration_resource(&self, key: &str) -> Result<ConfigurationResource> {
        let access = self
            .configuration
            .as_ref()
            .ok_or_else(|| NuonoError::NotImplemented("数据库未连接插件配置能力".into()))?;
        let definition = access.configuration_resource_definition(key).await?;
        let value = self
            .configuration_resource(key)
            .await?
            .unwrap_or(definition.initial()?);
        definition.check(&value)?;
        Ok(value)
    }
    pub async fn replace_configuration_resource(
        &self,
        key: &str,
        expected_revision: i64,
        value: serde_json::Value,
    ) -> Result<ConfigurationResource> {
        let access = self
            .configuration
            .as_ref()
            .ok_or_else(|| NuonoError::NotImplemented("数据库未连接插件配置能力".into()))?;
        let definition = access.configuration_resource_definition(key).await?;
        let plugin = self
            .plugin_id
            .as_deref()
            .ok_or_else(|| NuonoError::PermissionDenied("结构配置需要插件作用域".into()))?;
        let validated = definition.validated_write(plugin, expected_revision, value)?;
        self.save_configuration_resource(&validated).await
    }
}
