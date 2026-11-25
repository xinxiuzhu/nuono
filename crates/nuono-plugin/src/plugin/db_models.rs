//! 按插件命名空间隔离的键值存储（KV）封装，底层持久化由宿主数据库提供。
use crate::db::{Database, PluginDataStore};
use crate::error::Result;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone)]
pub struct PluginKVStore {
    db: Database,
    namespace: String,
}
impl PluginKVStore {
    pub fn new(db: impl PluginDataStore + 'static, namespace: impl Into<String>) -> Self {
        Self {
            db: Database::new(db),
            namespace: namespace.into(),
        }
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub(crate) fn from_database(db: Database, namespace: impl Into<String>) -> Self {
        Self {
            db,
            namespace: namespace.into(),
        }
    }
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        self.db.kv_get(&self.namespace, key).await
    }
    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        self.db.kv_set(&self.namespace, key, value).await
    }
    pub async fn delete(&self, key: &str) -> Result<bool> {
        self.db.kv_delete(&self.namespace, key).await
    }
    pub async fn list(&self) -> Result<Vec<(String, String)>> {
        self.db.kv_list(&self.namespace).await
    }
    pub async fn clear(&self) -> Result<u64> {
        self.db.kv_clear(&self.namespace).await
    }
    pub async fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.get(key).await?.is_some())
    }
    pub async fn get_as<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        self.get(key)
            .await?
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }
    pub async fn set_as<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.set(key, &serde_json::to_string(value)?).await
    }
    pub async fn list_as<T: for<'de> Deserialize<'de>>(&self) -> Result<Vec<(String, T)>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter_map(|(key, value)| serde_json::from_str(&value).ok().map(|value| (key, value)))
            .collect())
    }
}
